use std::fmt::{self, Write};

use super::{
    AwkError,
    eval::{Text, Value},
    parse::Refusal,
};
use crate::limits::Budget;

#[derive(Clone, Copy, Default)]
struct Flags {
    left: bool,
    plus: bool,
    space: bool,
    alternate: bool,
    zero: bool,
}

#[derive(Clone, Copy)]
enum Conversion {
    Character,
    String,
    Percent,
    Signed,
    Unsigned,
    Octal,
    Hex(bool),
    Scientific(bool),
    Fixed(bool),
    General(bool),
}

struct Spec {
    flags: Flags,
    width: usize,
    precision: Option<usize>,
    conversion: Conversion,
}

enum Piece<'a> {
    Literal(&'a str),
    Conversion(Spec),
}

struct Format<'a> {
    source: &'a str,
    function: &'a str,
    at: usize,
}

impl<'a> Format<'a> {
    fn new(function: &'a str, source: &'a str) -> Self {
        Self {
            source,
            function,
            at: 0,
        }
    }
    fn dimension(&mut self, dimension: &str) -> Result<usize, AwkError> {
        let start = self.at;
        let mut value = 0_usize;
        while let Some(c) = self
            .source
            .as_bytes()
            .get(self.at)
            .filter(|c| c.is_ascii_digit())
        {
            value = value
                .saturating_mul(10)
                .saturating_add(usize::from(*c - b'0'));
            self.at += 1;
        }
        if self.source.as_bytes().get(self.at) == Some(&b'*') {
            return Err(Refusal::FormatStar.into());
        }
        if value > 4096 {
            return Err(Refusal::FormatBound {
                function: self.function.into(),
                dimension: dimension.into(),
                value: self.source[start..self.at].into(),
            }
            .into());
        }
        Ok(value)
    }
    fn next(&mut self) -> Result<Option<Piece<'a>>, AwkError> {
        if self.at == self.source.len() {
            return Ok(None);
        }
        let start = self.at;
        if self.source.as_bytes()[start] != b'%' {
            self.at += self.source[start..]
                .find('%')
                .unwrap_or(self.source.len() - start);
            return Ok(Some(Piece::Literal(&self.source[start..self.at])));
        }
        self.at += 1;
        let mut flags = Flags::default();
        while let Some(c) = self.source.as_bytes().get(self.at) {
            match c {
                b'-' => flags.left = true,
                b'+' => flags.plus = true,
                b' ' => flags.space = true,
                b'#' => flags.alternate = true,
                b'0' => flags.zero = true,
                _ => break,
            }
            self.at += 1;
        }
        let width = self.dimension("width")?;
        let precision = if self.source.as_bytes().get(self.at) == Some(&b'.') {
            self.at += 1;
            Some(self.dimension("precision")?)
        } else {
            None
        };
        let Some(c) = self.source[self.at..].chars().next() else {
            return Err(AwkError::Syntax {
                line: 1,
                column: self.source.chars().count() + 1,
                expected: "a conversion after `%`".into(),
            });
        };
        self.at += c.len_utf8();
        let conversion = match c {
            'c' => Conversion::Character,
            's' => Conversion::String,
            '%' => Conversion::Percent,
            'd' | 'i' => Conversion::Signed,
            'u' => Conversion::Unsigned,
            'o' => Conversion::Octal,
            'x' | 'X' => Conversion::Hex(c == 'X'),
            'e' | 'E' => Conversion::Scientific(c == 'E'),
            'f' | 'F' => Conversion::Fixed(c == 'F'),
            'g' | 'G' => Conversion::General(c == 'G'),
            '*' => return Err(Refusal::FormatStar.into()),
            _ => return Err(Refusal::FormatConversion(c).into()),
        };
        Ok(Some(Piece::Conversion(Spec {
            flags,
            width,
            precision,
            conversion,
        })))
    }
}

pub(super) fn validate_format(function: &str, source: &str) -> Result<(), AwkError> {
    let mut format = Format::new(function, source);
    while format.next()?.is_some() {}
    Ok(())
}

struct Number {
    bytes: [u8; 4608],
    len: usize,
}

impl Number {
    fn new() -> Self {
        Self {
            bytes: [0; 4608],
            len: 0,
        }
    }
    fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("numeric ASCII")
    }
    fn trim_fraction(&mut self) {
        if self.bytes[..self.len].contains(&b'.') {
            while self.bytes.get(self.len.wrapping_sub(1)) == Some(&b'0') {
                self.len -= 1;
            }
            if self.bytes.get(self.len.wrapping_sub(1)) == Some(&b'.') {
                self.len -= 1;
            }
        }
    }
    fn decimal_point(&mut self) {
        if !self.bytes[..self.len].contains(&b'.') {
            self.write_str(".").expect("bounded number");
        }
    }
}

impl Write for Number {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let target = self
            .bytes
            .get_mut(self.len..self.len + text.len())
            .ok_or(fmt::Error)?;
        target.copy_from_slice(text.as_bytes());
        self.len += text.len();
        Ok(())
    }
}

fn scientific(out: &mut Number, number: f64, precision: usize, upper: bool, alternate: bool) {
    let mut raw = Number::new();
    write!(raw, "{number:.precision$e}").expect("bounded scientific number");
    let (mantissa, exponent) = raw
        .text()
        .split_once('e')
        .expect("Rust scientific notation");
    out.write_str(mantissa).expect("bounded mantissa");
    if alternate && precision == 0 {
        out.decimal_point();
    }
    let exponent: i32 = exponent.parse().expect("Rust exponent");
    write!(out, "{}{exponent:+03}", if upper { 'E' } else { 'e' }).expect("bounded exponent");
}

fn general(out: &mut Number, number: f64, precision: usize, upper: bool, alternate: bool) {
    let precision = precision.max(1);
    let mut raw = Number::new();
    write!(raw, "{number:.digits$e}", digits = precision - 1).expect("bounded significant digits");
    let (mantissa, exponent) = raw
        .text()
        .split_once('e')
        .expect("Rust scientific notation");
    let exponent: i32 = exponent.parse().expect("Rust exponent");
    if exponent < -4 || exponent >= precision as i32 {
        out.write_str(mantissa).expect("bounded mantissa");
        if alternate {
            out.decimal_point();
        } else {
            out.trim_fraction();
        }
        write!(out, "{}{exponent:+03}", if upper { 'E' } else { 'e' }).expect("bounded exponent");
    } else {
        let digits = (precision as i32 - exponent - 1).max(0) as usize;
        write!(out, "{number:.digits$}").expect("bounded fixed number");
        if alternate {
            out.decimal_point();
        } else {
            out.trim_fraction();
        }
    }
}

pub(super) fn number(number: f64, budget: &Budget) -> Result<Text, AwkError> {
    budget.check_deadline()?;
    let mut out = Number::new();
    if number.fract() == 0.0 && number.abs() < 9223372036854775808.0 {
        write!(out, "{}", number as i64).expect("bounded integer");
    } else {
        floating(
            &mut out,
            number,
            Conversion::General(false),
            6,
            Flags::default(),
        );
    }
    Text::new(out.text(), budget)
}

fn floating(out: &mut Number, number: f64, conversion: Conversion, precision: usize, flags: Flags) {
    if number.is_sign_negative() {
        out.write_str("-").expect("sign");
    } else if flags.plus {
        out.write_str("+").expect("sign");
    } else if flags.space {
        out.write_str(" ").expect("sign");
    }
    let upper = match conversion {
        Conversion::Scientific(upper) | Conversion::Fixed(upper) | Conversion::General(upper) => {
            upper
        }
        Conversion::Signed
        | Conversion::Unsigned
        | Conversion::Octal
        | Conversion::Hex(_)
        | Conversion::Character
        | Conversion::String
        | Conversion::Percent => false,
    };
    if !number.is_finite() {
        out.write_str(match (number.is_nan(), upper) {
            (true, true) => "NAN",
            (true, false) => "nan",
            (false, true) => "INF",
            (false, false) => "inf",
        })
        .expect("nonfinite number");
        return;
    }
    match conversion {
        Conversion::Scientific(upper) => {
            scientific(out, number.abs(), precision, upper, flags.alternate)
        }
        Conversion::General(upper) => general(out, number.abs(), precision, upper, flags.alternate),
        Conversion::Fixed(_) => {
            write!(out, "{:.precision$}", number.abs()).expect("bounded fixed number");
            if flags.alternate {
                out.decimal_point();
            }
        }
        Conversion::Signed
        | Conversion::Unsigned
        | Conversion::Octal
        | Conversion::Hex(_)
        | Conversion::Character
        | Conversion::String
        | Conversion::Percent => unreachable!("floating conversion"),
    }
}

fn integer(out: &mut Number, number: f64, spec: &Spec) -> usize {
    let mut digits = Number::new();
    let unsigned = if number < 0.0 {
        number as i64 as u64
    } else {
        number as u64
    };
    match spec.conversion {
        Conversion::Signed => {
            if number.is_sign_negative() && number.trunc() != 0.0 {
                out.write_str("-").expect("sign");
            } else if spec.flags.plus {
                out.write_str("+").expect("sign");
            } else if spec.flags.space {
                out.write_str(" ").expect("sign");
            }
            write!(digits, "{:.0}", number.trunc().abs()).expect("bounded decimal integer");
        }
        Conversion::Unsigned => write!(digits, "{unsigned}").expect("u64"),
        Conversion::Octal => write!(digits, "{unsigned:o}").expect("octal u64"),
        Conversion::Hex(upper) => {
            if spec.flags.alternate && unsigned != 0 {
                out.write_str(if upper { "0X" } else { "0x" })
                    .expect("prefix");
            }
            if upper {
                write!(digits, "{unsigned:X}").expect("hex u64");
            } else {
                write!(digits, "{unsigned:x}").expect("hex u64");
            }
        }
        Conversion::Scientific(_)
        | Conversion::Fixed(_)
        | Conversion::General(_)
        | Conversion::Character
        | Conversion::String
        | Conversion::Percent => unreachable!("integer conversion"),
    }
    if spec.precision == Some(0) && digits.text() == "0" {
        digits.len = 0;
    }
    let prefix = out.len;
    let zeros = spec.precision.unwrap_or(0).saturating_sub(digits.len);
    for _ in 0..zeros {
        out.write_str("0").expect("bounded precision");
    }
    if let Conversion::Octal = spec.conversion
        && spec.flags.alternate
        && zeros == 0
        && !digits.text().starts_with('0')
    {
        out.write_str("0").expect("octal prefix");
    }
    out.write_str(digits.text()).expect("bounded integer");
    prefix
}

fn padded<E: From<AwkError>>(
    text: &str,
    prefix: usize,
    spec: &Spec,
    zeros: bool,
    budget: &mut Budget,
    emit: &mut impl FnMut(&str, &mut Budget) -> Result<bool, E>,
) -> Result<bool, E> {
    static SPACES: [u8; 4096] = [b' '; 4096];
    static ZEROS: [u8; 4096] = [b'0'; 4096];
    let padding = spec.width.saturating_sub(text.chars().count());
    let pad = std::str::from_utf8(if zeros && !spec.flags.left {
        &ZEROS[..padding]
    } else {
        &SPACES[..padding]
    })
    .expect("ASCII padding");
    if spec.flags.left {
        return Ok(emit(text, budget)? && emit(pad, budget)?);
    }
    if zeros {
        Ok(emit(&text[..prefix], budget)? && emit(pad, budget)? && emit(&text[prefix..], budget)?)
    } else {
        Ok(emit(pad, budget)? && emit(text, budget)?)
    }
}

fn conversion<E: From<AwkError>>(
    spec: &Spec,
    value: &Value,
    budget: &mut Budget,
    emit: &mut impl FnMut(&str, &mut Budget) -> Result<bool, E>,
) -> Result<bool, E> {
    budget.check_deadline().map_err(AwkError::from)?;
    let mut out = Number::new();
    let (prefix, zeros) = match spec.conversion {
        Conversion::String => {
            let text = value.text(budget)?;
            let end = spec
                .precision
                .and_then(|n| text.as_str().char_indices().nth(n).map(|(at, _)| at))
                .unwrap_or(text.as_str().len());
            return padded(&text.as_str()[..end], 0, spec, false, budget, emit);
        }
        Conversion::Character => {
            let c = match value {
                Value::Num(n) | Value::StrNum(_, n) => {
                    if n.is_finite() && *n >= 0.0 && *n <= 0x10ffff as f64 {
                        char::from_u32(n.trunc() as u32).unwrap_or('\u{fffd}')
                    } else {
                        '\u{fffd}'
                    }
                }
                Value::Str(text) => text.as_str().chars().next().unwrap_or('\0'),
                Value::Uninit => '\0',
            };
            out.write_char(c).expect("one character");
            (0, false)
        }
        Conversion::Percent => {
            out.write_str("%").expect("percent");
            (0, false)
        }
        Conversion::Signed | Conversion::Unsigned | Conversion::Octal | Conversion::Hex(_) => {
            let prefix = integer(&mut out, value.number(), spec);
            (prefix, spec.flags.zero && spec.precision.is_none())
        }
        kind @ (Conversion::Scientific(_) | Conversion::Fixed(_) | Conversion::General(_)) => {
            let n = value.number();
            floating(&mut out, n, kind, spec.precision.unwrap_or(6), spec.flags);
            (
                usize::from(n.is_sign_negative() || spec.flags.plus || spec.flags.space),
                spec.flags.zero && n.is_finite(),
            )
        }
    };
    padded(out.text(), prefix, spec, zeros, budget, emit)
}

pub(super) fn write<E: From<AwkError>>(
    function: &str,
    source: &str,
    args: &[Value],
    budget: &mut Budget,
    mut emit: impl FnMut(&str, &mut Budget) -> Result<bool, E>,
) -> Result<bool, E> {
    budget.check_deadline().map_err(AwkError::from)?;
    validate_format(function, source)?;
    let mut format = Format::new(function, source);
    let mut argument = 0;
    while let Some(piece) = format.next()? {
        let accepted = match piece {
            Piece::Literal(text) => emit(text, budget)?,
            Piece::Conversion(spec) => {
                let value = match spec.conversion {
                    Conversion::Percent => &Value::Uninit,
                    Conversion::Character
                    | Conversion::String
                    | Conversion::Signed
                    | Conversion::Unsigned
                    | Conversion::Octal
                    | Conversion::Hex(_)
                    | Conversion::Scientific(_)
                    | Conversion::Fixed(_)
                    | Conversion::General(_) => {
                        let value = args.get(argument).unwrap_or(&Value::Uninit);
                        argument += 1;
                        value
                    }
                };
                conversion(&spec, value, budget, &mut emit)?
            }
        };
        if !accepted {
            return Ok(false);
        }
    }
    Ok(true)
}

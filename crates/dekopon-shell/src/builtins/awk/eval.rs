use std::{
    collections::{BTreeMap, VecDeque},
    ops::Range,
    rc::Rc,
};

use super::parse::{Binary, Expr, ExprKind, Function, Phase, Stmt, StmtKind, Unary};
use super::{AwkError, ChargedRegex, Compiled, RunError, RuntimeError, RuntimeKind};
use crate::{builtins::CommandFailure, limits::Budget};
use dekopon_process::{LimitExceeded, RetainedBytes};

#[derive(Clone, Debug)]
pub(super) struct Text(Rc<StoredText>);

#[derive(Debug)]
struct StoredText {
    text: String,
    _charge: RetainedBytes,
}

impl Text {
    pub fn new(source: &str, budget: &Budget) -> Result<Self, AwkError> {
        Self::build(source.len(), budget, || source.to_owned())
    }
    pub fn build(
        size: usize,
        budget: &Budget,
        build: impl FnOnce() -> String,
    ) -> Result<Self, AwkError> {
        budget.check_deadline()?;
        let mut charge = budget.charge_value_bytes(size as u64)?;
        let text = build();
        debug_assert!(text.len() <= size);
        charge.shrink(size.saturating_sub(text.len()) as u64);
        Ok(Self::owned(text, charge))
    }
    pub fn owned(text: String, charge: RetainedBytes) -> Self {
        Self(Rc::new(StoredText {
            text,
            _charge: charge,
        }))
    }
    pub fn as_str(&self) -> &str {
        &self.0.text
    }
}

struct Builder {
    text: String,
    charge: RetainedBytes,
}

impl Builder {
    fn new(budget: &Budget) -> Result<Self, AwkError> {
        Ok(Self {
            text: String::new(),
            charge: budget.charge_value_bytes(0)?,
        })
    }
    fn append(&mut self, text: &str, budget: &Budget) -> Result<(), AwkError> {
        budget.check_deadline()?;
        self.charge.grow(text.len() as u64)?;
        self.text.push_str(text);
        Ok(())
    }
    fn finish(self) -> Text {
        Text::owned(self.text, self.charge)
    }

    fn replacement(
        &mut self,
        replacement: &str,
        matched: &str,
        budget: &Budget,
    ) -> Result<(), AwkError> {
        let mut chars = replacement.char_indices().peekable();
        let mut plain = 0;
        while let Some((at, c)) = chars.next() {
            if c == '&' {
                self.append(&replacement[plain..at], budget)?;
                self.append(matched, budget)?;
                plain = at + 1;
            } else if c == '\\'
                && let Some((next, escaped @ ('&' | '\\'))) = chars.next()
            {
                self.append(&replacement[plain..at], budget)?;
                self.append(if escaped == '&' { "&" } else { "\\" }, budget)?;
                plain = next + 1;
            }
        }
        self.append(&replacement[plain..], budget)
    }
}

#[derive(Clone, Debug)]
pub(super) enum Value {
    Num(f64),
    Str(Text),
    StrNum(Text, f64),
    Uninit,
}

pub(super) fn number_prefix(source: &str) -> (f64, usize) {
    let bytes = source.as_bytes();
    let mut at = 0;
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    let start = at;
    if bytes.get(at).is_some_and(|c| *c == b'+' || *c == b'-') {
        at += 1;
    }
    let digits = at;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    let mut count = at - digits;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let fraction = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        count += at - fraction;
    }
    if count == 0 {
        return (0.0, 0);
    }
    let mantissa_end = at;
    if bytes.get(at).is_some_and(|c| *c == b'e' || *c == b'E') {
        at += 1;
        if bytes.get(at).is_some_and(|c| *c == b'+' || *c == b'-') {
            at += 1;
        }
        let exponent = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == exponent {
            at = mantissa_end;
        }
    }
    (source[start..at].parse().unwrap_or(0.0), at)
}

impl Value {
    pub fn input(text: Text) -> Self {
        let (number, end) = number_prefix(text.as_str());
        if end != 0
            && text.as_str()[end..]
                .bytes()
                .all(|c| c.is_ascii_whitespace())
        {
            Self::StrNum(text, number)
        } else {
            Self::Str(text)
        }
    }
    fn number(&self) -> f64 {
        match self {
            Self::Num(number) | Self::StrNum(_, number) => *number,
            Self::Str(text) => number_prefix(text.as_str()).0,
            Self::Uninit => 0.0,
        }
    }
    fn truth(&self) -> bool {
        match self {
            Self::Num(number) | Self::StrNum(_, number) => *number != 0.0,
            Self::Str(text) => !text.as_str().is_empty(),
            Self::Uninit => false,
        }
    }
    pub fn text(&self, budget: &Budget) -> Result<Text, AwkError> {
        match self {
            Self::Str(text) | Self::StrNum(text, _) => Ok(text.clone()),
            Self::Uninit => Text::new("", budget),
            Self::Num(number) => Text::build(512, budget, || format!("{number}")),
        }
    }
    fn compare(
        &self,
        right: &Self,
        budget: &Budget,
    ) -> Result<Option<std::cmp::Ordering>, AwkError> {
        budget.check_deadline()?;
        let numeric = match (self, right) {
            (
                Self::Num(_) | Self::StrNum(_, _) | Self::Uninit,
                Self::Num(_) | Self::StrNum(_, _) | Self::Uninit,
            ) => true,
            (Self::Str(_), Self::Num(_) | Self::Str(_) | Self::StrNum(_, _) | Self::Uninit)
            | (Self::Num(_) | Self::StrNum(_, _) | Self::Uninit, Self::Str(_)) => false,
        };
        Ok(if numeric {
            self.number().partial_cmp(&right.number())
        } else {
            Some(
                self.text(budget)?
                    .as_str()
                    .cmp(right.text(budget)?.as_str()),
            )
        })
    }
}

struct Slot {
    value: Value,
    _charge: RetainedBytes,
}

struct Array {
    elements: BTreeMap<String, Slot>,
    _charge: RetainedBytes,
}

pub(super) enum Pattern {
    Literal(usize),
    Dynamic(Text),
}

enum Separator {
    Space,
    Character(char),
    Regex(Pattern),
}

enum Place<'a> {
    Variable(&'a str),
    Field(usize),
    Element(&'a str, Text),
}

#[derive(Debug, PartialEq)]
pub(super) enum Flow {
    Normal,
    Next,
    Exit(u8),
    Break,
    Continue,
}

pub(super) struct Engine<'a> {
    compiled: &'a Compiled,
    pub budget: &'a mut Budget,
    variables: BTreeMap<String, Slot>,
    arrays: BTreeMap<String, Array>,
    fields: Vec<Slot>,
    record: Value,
    pub nr: f64,
    fs: Value,
    ofs: Value,
    ors: Value,
    rstart: f64,
    rlength: f64,
    cache: VecDeque<(Text, ChargedRegex)>,
    ranges: Vec<bool>,
    _range_charge: RetainedBytes,
    pub status: u8,
    pub reader_gone: bool,
}

impl<'a> Engine<'a> {
    pub fn new(compiled: &'a Compiled, budget: &'a mut Budget) -> Result<Self, AwkError> {
        let range_charge = budget.charge_value_bytes(compiled.program.rules.len() as u64)?;
        Ok(Self {
            compiled,
            variables: BTreeMap::new(),
            arrays: BTreeMap::new(),
            fields: Vec::new(),
            record: Value::Str(Text::new("", budget)?),
            nr: 0.0,
            fs: Value::Str(Text::new(" ", budget)?),
            ofs: Value::Str(Text::new(" ", budget)?),
            ors: Value::Str(Text::new("\n", budget)?),
            rstart: 0.0,
            rlength: -1.0,
            cache: VecDeque::new(),
            ranges: vec![false; compiled.program.rules.len()],
            _range_charge: range_charge,
            status: 0,
            reader_gone: false,
            budget,
        })
    }
    pub fn error(&self, kind: RuntimeKind) -> AwkError {
        AwkError::Runtime(RuntimeError {
            kind,
            record: self.nr,
        })
    }
    fn size(&self, size: Option<usize>) -> Result<usize, AwkError> {
        size.ok_or_else(|| {
            LimitExceeded::ValueBytes {
                maximum: self.budget.max_value_bytes(),
            }
            .into()
        })
    }
    fn field_index(&self, value: f64) -> Result<usize, AwkError> {
        let index = value.trunc();
        if index < 0.0 || index > 65536.0 {
            return Err(self.error(RuntimeKind::FieldIndex(index)));
        }
        Ok(index as usize)
    }
    fn get_variable(&self, name: &str) -> Value {
        match name {
            "NF" => Value::Num(self.fields.len() as f64),
            "NR" | "FNR" => Value::Num(self.nr),
            "FS" => self.fs.clone(),
            "OFS" => self.ofs.clone(),
            "ORS" => self.ors.clone(),
            "RSTART" => Value::Num(self.rstart),
            "RLENGTH" => Value::Num(self.rlength),
            _ => self
                .variables
                .get(name)
                .map_or(Value::Uninit, |slot| slot.value.clone()),
        }
    }
    pub fn set_variable(&mut self, name: &str, value: Value) -> Result<(), AwkError> {
        match name {
            "NF" => {
                let number = value.number().trunc();
                if number > 65536.0 {
                    return Err(self.error(RuntimeKind::Fields));
                }
                let count = self.field_index(number)?;
                self.resize_fields(count)?;
                self.rebuild()?;
            }
            "NR" | "FNR" => self.nr = value.number(),
            "FS" => {
                self.separator(value.text(self.budget)?)?;
                self.fs = value;
            }
            "OFS" => self.ofs = value,
            "ORS" => self.ors = value,
            "RSTART" => self.rstart = value.number(),
            "RLENGTH" => self.rlength = value.number(),
            _ => {
                if let Some(slot) = self.variables.get_mut(name) {
                    slot.value = value;
                } else {
                    let charge = self.budget.charge_value_bytes(name.len() as u64 + 64)?;
                    self.variables.insert(
                        name.to_owned(),
                        Slot {
                            value,
                            _charge: charge,
                        },
                    );
                }
            }
        }
        Ok(())
    }
    fn array(&mut self, name: &str) -> Result<&mut Array, AwkError> {
        if !self.arrays.contains_key(name) {
            let charge = self.budget.charge_value_bytes(name.len() as u64 + 64)?;
            self.arrays.insert(
                name.to_owned(),
                Array {
                    elements: BTreeMap::new(),
                    _charge: charge,
                },
            );
        }
        Ok(self.arrays.get_mut(name).expect("inserted array"))
    }
    fn set_element(&mut self, name: &str, key: &str, value: Value) -> Result<(), AwkError> {
        if let Some(slot) = self.array(name)?.elements.get_mut(key) {
            slot.value = value;
        } else {
            let charge = self.budget.charge_value_bytes(key.len() as u64 + 64)?;
            self.array(name)?.elements.insert(
                key.to_owned(),
                Slot {
                    value,
                    _charge: charge,
                },
            );
        }
        Ok(())
    }
    fn get_element(&mut self, name: &str, key: &str) -> Result<Value, AwkError> {
        if !self.array(name)?.elements.contains_key(key) {
            self.set_element(name, key, Value::Uninit)?;
        }
        Ok(self
            .array(name)?
            .elements
            .get(key)
            .expect("inserted element")
            .value
            .clone())
    }
    fn place<'e>(&mut self, expr: &'e Expr) -> Result<Place<'e>, AwkError> {
        match &expr.kind {
            ExprKind::Variable(name) => Ok(Place::Variable(name)),
            ExprKind::Array(name, index) => Ok(Place::Element(
                name,
                self.expression(index)?.text(self.budget)?,
            )),
            ExprKind::Field(index) => {
                let n = self.expression(index)?.number();
                Ok(Place::Field(self.field_index(n)?))
            }
            ExprKind::Group(inner) => self.place(inner),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => unreachable!("parser validates assignable expressions"),
        }
    }
    fn get(&mut self, place: &Place<'_>) -> Result<Value, AwkError> {
        match place {
            Place::Variable(name) => Ok(self.get_variable(name)),
            Place::Element(name, key) => self.get_element(name, key.as_str()),
            Place::Field(0) => Ok(self.record.clone()),
            Place::Field(index) => match self.fields.get(index - 1) {
                Some(slot) => Ok(slot.value.clone()),
                None => Ok(Value::Str(Text::new("", self.budget)?)),
            },
        }
    }
    fn put(&mut self, place: Place<'_>, value: Value) -> Result<(), AwkError> {
        match place {
            Place::Variable(name) => self.set_variable(name, value),
            Place::Element(name, key) => self.set_element(name, key.as_str(), value),
            Place::Field(0) => self.set_record(value),
            Place::Field(index) => {
                self.resize_fields(self.fields.len().max(index))?;
                self.fields[index - 1].value = value;
                self.rebuild()
            }
        }
    }
    fn resize_fields(&mut self, count: usize) -> Result<(), AwkError> {
        self.fields.truncate(count);
        while self.fields.len() < count {
            let charge = self.budget.charge_value_bytes(32)?;
            let value = Value::Str(Text::new("", self.budget)?);
            self.fields.push(Slot {
                value,
                _charge: charge,
            });
        }
        Ok(())
    }
    fn rebuild(&mut self) -> Result<(), AwkError> {
        self.budget.check_deadline()?;
        let ofs = self.ofs.text(self.budget)?;
        let mut size = self.size(
            self.fields
                .len()
                .saturating_sub(1)
                .checked_mul(ofs.as_str().len()),
        )?;
        for field in &self.fields {
            size = self.size(size.checked_add(field.value.text(self.budget)?.as_str().len()))?;
        }
        let charge = self.budget.charge_value_bytes(size as u64)?;
        let mut text = String::with_capacity(size);
        for (index, field) in self.fields.iter().enumerate() {
            if index != 0 {
                text.push_str(ofs.as_str());
            }
            text.push_str(field.value.text(self.budget)?.as_str());
        }
        self.record = Value::Str(Text::owned(text, charge));
        Ok(())
    }
    pub fn set_record(&mut self, value: Value) -> Result<(), AwkError> {
        let text = value.text(self.budget)?;
        let separator = self.separator(self.fs.text(self.budget)?)?;
        self.fields.clear();
        self.fields = self.split(text.as_str(), &separator)?;
        self.record = value;
        Ok(())
    }
    pub fn seed_regex(&mut self, source: Text, regex: ChargedRegex) {
        if self.cache.len() == 16 {
            self.cache.pop_front();
        }
        self.cache.push_back((source, regex));
    }
    fn cached(&mut self, source: &Text) -> Result<usize, AwkError> {
        if let Some(index) = self
            .cache
            .iter()
            .position(|(key, _)| key.as_str() == source.as_str())
        {
            let entry = self.cache.remove(index).expect("cache position exists");
            self.cache.push_back(entry);
        } else {
            if self.cache.len() == 16 {
                self.cache.pop_front();
            }
            self.budget.check_deadline()?;
            let regex =
                ChargedRegex::compile(source.as_str(), self.budget).map_err(
                    |error| match error {
                        AwkError::Refused(refusal) => self.error(RuntimeKind::Refused(refusal)),
                        other @ (AwkError::Syntax { .. }
                        | AwkError::Runtime(_)
                        | AwkError::Limit(_)) => other,
                    },
                )?;
            self.cache.push_back((source.clone(), regex));
        }
        Ok(self.cache.len() - 1)
    }
    fn find(
        &mut self,
        pattern: &Pattern,
        text: &str,
        at: usize,
    ) -> Result<Option<Range<usize>>, AwkError> {
        let regex = match pattern {
            Pattern::Literal(index) => &self.compiled.regexes[*index].regex,
            Pattern::Dynamic(source) => {
                let index = self.cached(source)?;
                &self.cache[index].1.regex
            }
        };
        self.budget.check_deadline()?;
        Ok(regex.find_at(text, at).map(|found| found.range()))
    }
    fn pattern(&mut self, expression: &Expr) -> Result<Pattern, AwkError> {
        match &expression.kind {
            ExprKind::Regex(index) => Ok(Pattern::Literal(*index)),
            ExprKind::Group(inner) => self.pattern(inner),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => {
                let source = self.expression(expression)?.text(self.budget)?;
                self.cached(&source)?;
                Ok(Pattern::Dynamic(source))
            }
        }
    }
    fn separator(&mut self, text: Text) -> Result<Separator, AwkError> {
        if text.as_str() == " " {
            return Ok(Separator::Space);
        }
        let mut chars = text.as_str().chars();
        let Some(first) = chars.next() else {
            return Err(self.error(RuntimeKind::EmptySeparator));
        };
        if chars.next().is_none() {
            return Ok(Separator::Character(first));
        }
        self.cached(&text)?;
        Ok(Separator::Regex(Pattern::Dynamic(text)))
    }
    fn split(&mut self, text: &str, separator: &Separator) -> Result<Vec<Slot>, AwkError> {
        self.budget.check_deadline()?;
        let mut fields = Vec::new();
        if text.is_empty() {
            return Ok(fields);
        }
        match separator {
            Separator::Space => {
                for field in text.split([' ', '\t', '\n']).filter(|s| !s.is_empty()) {
                    self.push_field(&mut fields, field)?;
                }
            }
            Separator::Character(c) => {
                for field in text.split(*c) {
                    self.push_field(&mut fields, field)?;
                }
            }
            Separator::Regex(pattern) => {
                let (mut cursor, mut at) = (0, 0);
                while let Some(found) = self.find(pattern, text, at)? {
                    if found.is_empty() {
                        let Some(c) = text[found.end..].chars().next() else {
                            break;
                        };
                        at = found.end + c.len_utf8();
                    } else {
                        self.push_field(&mut fields, &text[cursor..found.start])?;
                        cursor = found.end;
                        at = found.end;
                    }
                }
                self.push_field(&mut fields, &text[cursor..])?;
            }
        }
        Ok(fields)
    }
    fn push_field(&self, fields: &mut Vec<Slot>, text: &str) -> Result<(), AwkError> {
        if fields.len() == 65536 {
            return Err(self.error(RuntimeKind::Fields));
        }
        let charge = self.budget.charge_value_bytes(32)?;
        let value = Value::input(Text::new(text, self.budget)?);
        fields.push(Slot {
            value,
            _charge: charge,
        });
        Ok(())
    }
    fn expression(&mut self, expression: &Expr) -> Result<Value, AwkError> {
        self.budget.check_deadline()?;
        match &expression.kind {
            ExprKind::Number(number) => Ok(Value::Num(*number)),
            ExprKind::String(text) => Ok(Value::Str(Text::new(text, self.budget)?)),
            ExprKind::Regex(index) => {
                let text = self.record.text(self.budget)?;
                Ok(Value::Num(f64::from(
                    self.find(&Pattern::Literal(*index), text.as_str(), 0)?
                        .is_some(),
                )))
            }
            ExprKind::Variable(_) | ExprKind::Array(_, _) | ExprKind::Field(_) => {
                let place = self.place(expression)?;
                self.get(&place)
            }
            ExprKind::Group(inner) => self.expression(inner),
            ExprKind::Unary(operator, inner) => {
                let value = self.expression(inner)?;
                Ok(Value::Num(match operator {
                    Unary::Not => f64::from(!value.truth()),
                    Unary::Plus => value.number(),
                    Unary::Minus => -value.number(),
                }))
            }
            ExprKind::Increment {
                target,
                delta,
                post,
            } => {
                let place = self.place(target)?;
                let old = self.get(&place)?.number();
                let new = old + f64::from(*delta);
                self.put(place, Value::Num(new))?;
                Ok(Value::Num(if *post { old } else { new }))
            }
            ExprKind::Assign(operator, target, right) => {
                let place = self.place(target)?;
                let value = self.expression(right)?;
                let value = match operator {
                    Some(op) => {
                        let old = self.get(&place)?;
                        self.binary(*op, old, value)?
                    }
                    None => value,
                };
                self.put(place, value.clone())?;
                Ok(value)
            }
            ExprKind::Binary(operator, left, right) => {
                let left = self.expression(left)?;
                self.right(*operator, left, right)
            }
            ExprKind::Chain { first, rest } => {
                let mut value = self.expression(first)?;
                for (operator, right) in rest {
                    value = self.right(*operator, value, right)?;
                }
                Ok(value)
            }
            ExprKind::Conditional(condition, yes, no) => {
                if self.expression(condition)?.truth() {
                    self.expression(yes)
                } else {
                    self.expression(no)
                }
            }
            ExprKind::In(key, name) => {
                let key = self.expression(key)?.text(self.budget)?;
                Ok(Value::Num(f64::from(
                    self.arrays
                        .get(name)
                        .is_some_and(|a| a.elements.contains_key(key.as_str())),
                )))
            }
            ExprKind::Call(function, args) => self.call(*function, args),
        }
    }
    fn right(&mut self, operator: Binary, left: Value, right: &Expr) -> Result<Value, AwkError> {
        if operator == Binary::And && !left.truth() {
            return Ok(Value::Num(0.0));
        }
        if operator == Binary::Or && left.truth() {
            return Ok(Value::Num(1.0));
        }
        if operator == Binary::Match || operator == Binary::NotMatch {
            let text = left.text(self.budget)?;
            let pattern = self.pattern(right)?;
            let found = self.find(&pattern, text.as_str(), 0)?.is_some();
            return Ok(Value::Num(f64::from(found == (operator == Binary::Match))));
        }
        let right = self.expression(right)?;
        self.binary(operator, left, right)
    }
    fn binary(&self, operator: Binary, left: Value, right: Value) -> Result<Value, AwkError> {
        self.budget.check_deadline()?;
        use std::cmp::Ordering::{Equal, Greater, Less};
        let number = match operator {
            Binary::Add => left.number() + right.number(),
            Binary::Sub => left.number() - right.number(),
            Binary::Mul => left.number() * right.number(),
            Binary::Pow => left.number().powf(right.number()),
            Binary::Div | Binary::Rem => {
                if right.number() == 0.0 {
                    return Err(self.error(RuntimeKind::DivisionByZero));
                }
                if operator == Binary::Div {
                    left.number() / right.number()
                } else {
                    left.number() % right.number()
                }
            }
            Binary::Or => f64::from(left.truth() || right.truth()),
            Binary::And => f64::from(left.truth() && right.truth()),
            Binary::Concat => {
                self.budget.check_deadline()?;
                let a = left.text(self.budget)?;
                let b = right.text(self.budget)?;
                let size = self.size(a.as_str().len().checked_add(b.as_str().len()))?;
                return Ok(Value::Str(Text::build(size, self.budget, || {
                    let mut text = String::with_capacity(size);
                    text.push_str(a.as_str());
                    text.push_str(b.as_str());
                    text
                })?));
            }
            Binary::Lt => f64::from(left.compare(&right, self.budget)? == Some(Less)),
            Binary::Le => {
                f64::from([Some(Less), Some(Equal)].contains(&left.compare(&right, self.budget)?))
            }
            Binary::Ne => f64::from(left.compare(&right, self.budget)? != Some(Equal)),
            Binary::Eq => f64::from(left.compare(&right, self.budget)? == Some(Equal)),
            Binary::Gt => f64::from(left.compare(&right, self.budget)? == Some(Greater)),
            Binary::Ge => f64::from(
                [Some(Greater), Some(Equal)].contains(&left.compare(&right, self.budget)?),
            ),
            Binary::Match | Binary::NotMatch => {
                unreachable!("match uses an unevaluated regex operand")
            }
        };
        Ok(Value::Num(number))
    }
}

impl Engine<'_> {
    pub fn phase(
        &mut self,
        phase: Phase,
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<Flow, RunError> {
        let compiled = self.compiled;
        for (index, rule) in compiled.program.rules.iter().enumerate() {
            if rule.phase != phase {
                continue;
            }
            if let Some(pattern) = &rule.pattern {
                let active = self.ranges[index] || self.expression(pattern)?.truth();
                if !active {
                    continue;
                }
                if let Some(end) = &rule.range_end {
                    self.ranges[index] = !self.expression(end)?.truth();
                }
            }
            let flow = match &rule.action {
                Some(action) => self.statement(action, emit)?,
                None => self.print(&[], emit)?,
            };
            match flow {
                Flow::Normal => {}
                Flow::Next | Flow::Exit(_) => return Ok(flow),
                Flow::Break | Flow::Continue => unreachable!("parser requires a containing loop"),
            }
        }
        Ok(Flow::Normal)
    }
    fn statement(
        &mut self,
        statement: &Stmt,
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<Flow, RunError> {
        self.budget.check_deadline()?;
        match &statement.kind {
            StmtKind::Block(statements) => {
                for statement in statements {
                    let flow = self.statement(statement, emit)?;
                    match flow {
                        Flow::Normal => {}
                        Flow::Next | Flow::Exit(_) | Flow::Break | Flow::Continue => {
                            return Ok(flow);
                        }
                    }
                }
            }
            StmtKind::Empty => {}
            StmtKind::Expression(expr) => {
                self.expression(expr)?;
            }
            StmtKind::Print(args) => return self.print(args, emit),
            StmtKind::Printf(_) => return Err(self.error(RuntimeKind::FormatterPending).into()),
            StmtKind::If { condition, yes, no } => {
                if self.expression(condition)?.truth() {
                    return self.statement(yes, emit);
                }
                if let Some(no) = no {
                    return self.statement(no, emit);
                }
            }
            StmtKind::While(condition, body) => {
                while self.expression(condition)?.truth() {
                    match self.statement(body, emit)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        flow @ (Flow::Next | Flow::Exit(_)) => return Ok(flow),
                    }
                    self.budget.charge_step()?;
                }
            }
            StmtKind::Do(body, condition) => loop {
                match self.statement(body, emit)? {
                    Flow::Normal | Flow::Continue => {}
                    Flow::Break => break,
                    flow @ (Flow::Next | Flow::Exit(_)) => return Ok(flow),
                }
                if !self.expression(condition)?.truth() {
                    break;
                }
                self.budget.charge_step()?;
            },
            StmtKind::For {
                init,
                condition,
                step,
                body,
            } => {
                if let Some(init) = init {
                    self.expression(init)?;
                }
                loop {
                    if let Some(condition) = condition
                        && !self.expression(condition)?.truth()
                    {
                        break;
                    }
                    match self.statement(body, emit)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        flow @ (Flow::Next | Flow::Exit(_)) => return Ok(flow),
                    }
                    if let Some(step) = step {
                        self.expression(step)?;
                    }
                    self.budget.charge_step()?;
                }
            }
            StmtKind::ForIn { key, array, body } => {
                let mut keys = Vec::new();
                let mut charge = self.budget.charge_value_bytes(0)?;
                if let Some(array) = self.arrays.get(array) {
                    for name in array.elements.keys() {
                        charge.grow(32)?;
                        keys.push(Text::new(name, self.budget)?);
                    }
                }
                for name in keys {
                    if self
                        .arrays
                        .get(array)
                        .is_some_and(|a| a.elements.contains_key(name.as_str()))
                    {
                        self.set_variable(key, Value::Str(name))?;
                        match self.statement(body, emit)? {
                            Flow::Normal | Flow::Continue => {}
                            Flow::Break => break,
                            flow @ (Flow::Next | Flow::Exit(_)) => return Ok(flow),
                        }
                    }
                    self.budget.charge_step()?;
                }
            }
            StmtKind::Break => return Ok(Flow::Break),
            StmtKind::Continue => return Ok(Flow::Continue),
            StmtKind::Next => return Ok(Flow::Next),
            StmtKind::Exit(value) => {
                if let Some(value) = value {
                    self.status = self.expression(value)?.number().trunc().rem_euclid(256.0) as u8;
                }
                return Ok(Flow::Exit(self.status));
            }
            StmtKind::Delete(name, key) => {
                let key = key
                    .as_ref()
                    .map(|expr| self.expression(expr)?.text(self.budget))
                    .transpose()?;
                if let Some(array) = self.arrays.get_mut(name) {
                    match key {
                        Some(key) => {
                            array.elements.remove(key.as_str());
                        }
                        None => array.elements.clear(),
                    }
                }
            }
        }
        Ok(Flow::Normal)
    }
    fn print(
        &mut self,
        args: &[Expr],
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<Flow, RunError> {
        let _charge = self
            .budget
            .charge_value_bytes(args.len().max(1) as u64 * 32)?;
        let values = if args.is_empty() {
            vec![self.record.clone()]
        } else {
            args.iter()
                .map(|arg| self.expression(arg))
                .collect::<Result<Vec<_>, _>>()?
        };
        let ofs = self.ofs.text(self.budget)?;
        let ors = self.ors.text(self.budget)?;
        for (index, value) in values.into_iter().enumerate() {
            if index != 0 && !self.emit(ofs.as_str(), emit)? {
                return Ok(Flow::Exit(self.status));
            }
            if !self.emit(value.text(self.budget)?.as_str(), emit)? {
                return Ok(Flow::Exit(self.status));
            }
        }
        if !self.emit(ors.as_str(), emit)? {
            return Ok(Flow::Exit(self.status));
        }
        Ok(Flow::Normal)
    }
    fn emit(
        &mut self,
        text: &str,
        emit: &mut impl FnMut(&[u8]) -> Result<bool, CommandFailure>,
    ) -> Result<bool, RunError> {
        let accepted = crate::builtins::text::sed::emit_chunks(text.as_bytes(), self.budget, emit)?;
        self.reader_gone |= !accepted;
        Ok(accepted)
    }
    fn call(&mut self, function: Function, args: &[Expr]) -> Result<Value, AwkError> {
        self.budget.check_deadline()?;
        match function {
            Function::Length => {
                let value = match args.first() {
                    None => self.record.clone(),
                    Some(expr) => {
                        if let Some(name) = expr.variable()
                            && self.compiled.program.arrays.contains(name)
                        {
                            return Ok(Value::Num(
                                self.arrays.get(name).map_or(0, |a| a.elements.len()) as f64,
                            ));
                        }
                        self.expression(expr)?
                    }
                };
                Ok(Value::Num(
                    value.text(self.budget)?.as_str().chars().count() as f64,
                ))
            }
            Function::Substr => self.substr(args),
            Function::Index => {
                let text = self.expression(&args[0])?.text(self.budget)?;
                let needle = self.expression(&args[1])?.text(self.budget)?;
                Ok(Value::Num(
                    text.as_str()
                        .find(needle.as_str())
                        .map_or(0, |at| text.as_str()[..at].chars().count() + 1)
                        as f64,
                ))
            }
            Function::Split => self.split_call(args),
            Function::Sub | Function::Gsub => self.substitute(args, function == Function::Gsub),
            Function::Match => {
                let text = self.expression(&args[0])?.text(self.budget)?;
                let pattern = self.pattern(&args[1])?;
                match self.find(&pattern, text.as_str(), 0)? {
                    Some(found) => {
                        self.rstart = (text.as_str()[..found.start].chars().count() + 1) as f64;
                        self.rlength = text.as_str()[found].chars().count() as f64;
                    }
                    None => {
                        self.rstart = 0.0;
                        self.rlength = -1.0;
                    }
                }
                Ok(Value::Num(self.rstart))
            }
            Function::Tolower | Function::Toupper => {
                let text = self.expression(&args[0])?.text(self.budget)?;
                let text = text.as_str();
                Ok(Value::Str(if function == Function::Tolower {
                    let size = text
                        .chars()
                        .flat_map(char::to_lowercase)
                        .map(char::len_utf8)
                        .sum();
                    Text::build(size, self.budget, || text.to_lowercase())?
                } else {
                    let size = text
                        .chars()
                        .flat_map(char::to_uppercase)
                        .map(char::len_utf8)
                        .sum();
                    Text::build(size, self.budget, || text.to_uppercase())?
                }))
            }
            Function::Sprintf => Err(self.error(RuntimeKind::FormatterPending)),
            Function::Int
            | Function::Sqrt
            | Function::Exp
            | Function::Log
            | Function::Sin
            | Function::Cos
            | Function::Atan2 => {
                let value = self.expression(&args[0])?.number();
                self.numeric(function, value, args)
            }
        }
    }
    fn numeric(
        &mut self,
        function: Function,
        value: f64,
        args: &[Expr],
    ) -> Result<Value, AwkError> {
        Ok(Value::Num(match function {
            Function::Int => value.trunc(),
            Function::Sqrt => value.sqrt(),
            Function::Exp => value.exp(),
            Function::Log => value.ln(),
            Function::Sin => value.sin(),
            Function::Cos => value.cos(),
            Function::Atan2 => value.atan2(self.expression(&args[1])?.number()),
            Function::Length
            | Function::Substr
            | Function::Index
            | Function::Split
            | Function::Sub
            | Function::Gsub
            | Function::Match
            | Function::Sprintf
            | Function::Tolower
            | Function::Toupper => unreachable!("string functions have dedicated evaluation"),
        }))
    }
    fn substr(&mut self, args: &[Expr]) -> Result<Value, AwkError> {
        let text = self.expression(&args[0])?.text(self.budget)?;
        let start = self.expression(&args[1])?.number().round();
        let length = text.as_str().chars().count();
        let end = match args.get(2) {
            Some(n) => start + self.expression(n)?.number().round() - 1.0,
            None => length as f64,
        };
        let begin = (start - 1.0).max(0.0).min(length as f64) as usize;
        let end = end.max(0.0).min(length as f64) as usize;
        let begin_byte = text
            .as_str()
            .char_indices()
            .nth(begin)
            .map_or(text.as_str().len(), |(at, _)| at);
        let end_byte = text
            .as_str()
            .char_indices()
            .nth(end.max(begin))
            .map_or(text.as_str().len(), |(at, _)| at);
        Ok(Value::Str(Text::new(
            &text.as_str()[begin_byte..end_byte],
            self.budget,
        )?))
    }
    fn split_call(&mut self, args: &[Expr]) -> Result<Value, AwkError> {
        let text = self.expression(&args[0])?.text(self.budget)?;
        let name = match &args[1].kind {
            ExprKind::Variable(name) => name,
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Group(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => unreachable!("parser validates split's array name"),
        };
        let separator = match args.get(2) {
            Some(expr) => self.split_separator(expr)?,
            None => self.separator(self.fs.text(self.budget)?)?,
        };
        self.array(name)?.elements.clear();
        let fields = self.split(text.as_str(), &separator)?;
        let count = fields.len();
        for (index, field) in fields.into_iter().enumerate() {
            let key = Value::Num((index + 1) as f64).text(self.budget)?;
            self.set_element(name, key.as_str(), field.value)?;
        }
        Ok(Value::Num(count as f64))
    }
    fn split_separator(&mut self, expr: &Expr) -> Result<Separator, AwkError> {
        match &expr.kind {
            ExprKind::Regex(index) => {
                if self.compiled.program.regex_sources[*index].is_empty() {
                    return Err(self.error(RuntimeKind::EmptySeparator));
                }
                Ok(Separator::Regex(Pattern::Literal(*index)))
            }
            ExprKind::Group(inner) => self.split_separator(inner),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => {
                let text = self.expression(expr)?.text(self.budget)?;
                self.separator(text)
            }
        }
    }
    fn substitute(&mut self, args: &[Expr], global: bool) -> Result<Value, AwkError> {
        let pattern = self.pattern(&args[0])?;
        let replacement = self.expression(&args[1])?.text(self.budget)?;
        let place = match args.get(2) {
            Some(expr) => self.place(expr)?,
            None => Place::Field(0),
        };
        let input = self.get(&place)?.text(self.budget)?;
        let text = input.as_str();
        let mut output = Builder::new(self.budget)?;
        let (mut cursor, mut at, mut count) = (0, 0, 0_usize);
        let mut nonempty_end = None;
        while let Some(found) = self.find(&pattern, text, at)? {
            if found.is_empty() && nonempty_end == Some(found.start) {
                let Some(c) = text[found.end..].chars().next() else {
                    break;
                };
                at = found.end + c.len_utf8();
                continue;
            }
            output.append(&text[cursor..found.start], self.budget)?;
            output.replacement(replacement.as_str(), &text[found.clone()], self.budget)?;
            count += 1;
            cursor = found.end;
            if !global {
                break;
            }
            if found.is_empty() {
                nonempty_end = None;
                let Some(c) = text[found.end..].chars().next() else {
                    break;
                };
                at = found.end + c.len_utf8();
            } else {
                at = found.end;
                nonempty_end = Some(found.end);
            }
        }
        if count != 0 {
            output.append(&text[cursor..], self.budget)?;
            self.put(place, Value::Str(output.finish()))?;
        }
        Ok(Value::Num(count as f64))
    }
}

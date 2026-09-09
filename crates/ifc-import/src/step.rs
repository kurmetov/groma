//! Reading the exchange structure ISO 10303-21 defines: the text an IFC file
//! is written in.
//!
//! The parse is one pass over the bytes into an entity table. Nothing here
//! knows what an `IFCWALL` is; that is [`crate::model`]'s business. What this
//! guarantees is that every instance the file declares is either in the table
//! or counted in [`Parsed::skipped`] - a value this cannot read is never
//! quietly turned into a default.

use std::collections::HashMap;
use std::sync::Arc;

/// One attribute of an entity instance.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `$`, an attribute the file leaves unset.
    Unset,
    /// `*`, an attribute derived by the schema rather than stated.
    Derived,
    /// `#123`, a reference to another instance.
    Reference(u64),
    Integer(i64),
    Real(f64),
    Text(Box<str>),
    /// `.T.`, `.ELEMENT.` - an enumeration or a boolean, without its dots.
    /// Shared like a type name: a file states a handful of these millions of
    /// times over.
    Enumeration(TypeName),
    /// The members of a list, held exactly rather than in a vector with room
    /// to grow: a file of this size is mostly lists of two or three, and the
    /// slack a vector keeps is the difference between reading one and not.
    List(Box<[Value]>),
    /// `IFCPOSITIVELENGTHMEASURE(3.5)`: a value wearing its type. The name is
    /// shared, for the same reason an entity's is.
    Typed(TypeName, Box<Value>),
}

impl Value {
    #[must_use]
    pub fn as_reference(&self) -> Option<u64> {
        match self {
            Self::Reference(id) => Some(*id),
            Self::Typed(_, inner) => inner.as_reference(),
            _ => None,
        }
    }

    /// A number, whichever way the file wrote it. A length written as an
    /// integer is still a length.
    #[must_use]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Real(value) => Some(*value),
            #[allow(clippy::cast_precision_loss)]
            // An IFC integer attribute is a count or an index, far inside the
            // range a double states exactly.
            Self::Integer(value) => Some(*value as f64),
            Self::Typed(_, inner) => inner.as_number(),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            Self::Typed(_, inner) => inner.as_integer(),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Typed(_, inner) => inner.as_text(),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_enumeration(&self) -> Option<&str> {
        match self {
            Self::Enumeration(value) => Some(value),
            Self::Typed(_, inner) => inner.as_enumeration(),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(values) => Some(&values[..]),
            Self::Typed(_, inner) => inner.as_list(),
            _ => None,
        }
    }
}

/// The name of an entity's type, held once per distinct name rather than
/// once per instance.
///
/// A file of nine million instances states only a few dozen type names between
/// them - 53 in SMALL's 9.5 million - so a `String` on every instance spent
/// two allocations and some fifty bytes each to record six bits of
/// information. This shares one allocation per name and keeps comparing
/// against a `&str` the way a `String` did.
///
/// Held as `Arc<String>` rather than `Arc<str>`: the latter is a fat pointer
/// and would cost sixteen bytes on every instance and inside every value that
/// names a type. One extra indirection per name, at a few dozen names, buys
/// eight bytes back on each of the millions that point at them - and is what
/// brings [`Value`] itself down to twenty-four.
#[derive(Clone, Debug)]
pub struct TypeName(Arc<String>);

impl TypeName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for TypeName {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TypeName {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.0)
    }
}

impl PartialEq for TypeName {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for TypeName {}

impl PartialEq<str> for TypeName {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

impl PartialEq<&str> for TypeName {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

/// Every distinct type name seen so far, keyed by the bytes it was written as.
///
/// Keyed by the raw spelling rather than the upper-cased result, so the common
/// case costs one lookup and no allocation at all. Two spellings of one name
/// would take two entries and hand back equal text, which at a few dozen names
/// is not worth a second pass to avoid.
#[derive(Default)]
struct Names<'bytes> {
    /// Type and typed-value names, upper-cased as they always were.
    types: HashMap<&'bytes [u8], Arc<String>>,
    /// Dotted enumerations, kept exactly as written. Held apart from the
    /// upper-cased names so sharing cannot change what a file said.
    enumerations: HashMap<&'bytes [u8], Arc<String>>,
}

impl<'bytes> Names<'bytes> {
    /// A type or typed-value name, upper-cased as this reader always did.
    fn intern(&mut self, raw: &'bytes [u8]) -> TypeName {
        if let Some(found) = self.types.get(raw) {
            return TypeName(Arc::clone(found));
        }
        let text = Arc::new(String::from_utf8_lossy(raw).to_uppercase());
        self.types.insert(raw, Arc::clone(&text));
        TypeName(text)
    }

    /// A dotted enumeration, exactly as the file wrote it.
    fn intern_exact(&mut self, raw: &'bytes [u8]) -> TypeName {
        if let Some(found) = self.enumerations.get(raw) {
            return TypeName(Arc::clone(found));
        }
        let text = Arc::new(String::from_utf8_lossy(raw).into_owned());
        self.enumerations.insert(raw, Arc::clone(&text));
        TypeName(text)
    }
}

/// One `#id = TYPE(...);` instance.
#[derive(Clone, Debug)]
pub struct Entity {
    pub type_name: TypeName,
    pub attributes: Box<[Value]>,
}

impl Entity {
    #[must_use]
    pub fn attribute(&self, at: usize) -> Option<&Value> {
        self.attributes
            .get(at)
            .filter(|value| !matches!(value, Value::Unset | Value::Derived))
    }
}

/// Everything a file declared.
pub struct Parsed {
    pub entities: HashMap<u64, Entity>,
    /// Instances whose text this could not read. They are counted rather than
    /// guessed at, so a caller can say what it did not see.
    pub skipped: usize,
    /// `FILE_NAME`'s originating system, where the header states one.
    pub application: Option<String>,
    /// `FILE_SCHEMA`, e.g. `IFC4`.
    pub schema: Option<String>,
}

impl Parsed {
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Entity> {
        self.entities.get(&id)
    }

    /// Follow a reference attribute to the entity it names.
    #[must_use]
    pub fn follow(&self, value: Option<&Value>) -> Option<&Entity> {
        self.get(value?.as_reference()?)
    }

    /// Every entity of one type, in file order.
    #[must_use]
    pub fn of_type(&self, type_name: &str) -> Vec<(u64, &Entity)> {
        let mut found: Vec<(u64, &Entity)> = self
            .entities
            .iter()
            .filter(|(_, entity)| entity.type_name == type_name)
            .map(|(id, entity)| (*id, entity))
            .collect();
        found.sort_unstable_by_key(|(id, _)| *id);
        found
    }
}

/// Read a whole file.
///
/// # Errors
///
/// Fails where the bytes are not an ISO 10303-21 exchange file.
pub fn parse(bytes: &[u8]) -> Result<Parsed, String> {
    let mut reader = Reader {
        bytes,
        at: 0,
        length: bytes.len(),
        names: Names::default(),
    };
    reader.skip_whitespace();
    if !reader.take_keyword("ISO-10303-21") {
        return Err("this is not an ISO 10303-21 file: it does not begin with ISO-10303-21".into());
    }

    let mut parsed = Parsed {
        // Sized from the source rather than grown into. Every doubling copies
        // the table and holds the old one while it does, so the last one alone
        // would add more to the peak than the entities it makes room for; 64
        // bytes an instance is what this corpus averages, and guessing a
        // little high costs a fraction of what one rehash does.
        entities: HashMap::with_capacity(bytes.len() / 64),
        skipped: 0,
        application: None,
        schema: None,
    };
    let mut in_data = false;
    loop {
        reader.skip_whitespace();
        if reader.at >= reader.length {
            break;
        }
        if reader.take_keyword("ENDSEC") {
            reader.take_semicolon();
            in_data = false;
            continue;
        }
        if reader.take_keyword("HEADER") {
            reader.take_semicolon();
            reader.read_header(&mut parsed);
            continue;
        }
        if reader.take_keyword("DATA") {
            // `DATA` may carry a parameter list before its semicolon.
            reader.skip_whitespace();
            if reader.peek() == Some(b'(') {
                let _ = reader.read_value();
            }
            reader.take_semicolon();
            in_data = true;
            continue;
        }
        if reader.take_keyword("END-ISO-10303-21") {
            break;
        }
        if in_data && reader.peek() == Some(b'#') {
            match reader.read_instance() {
                Some((id, entity)) => {
                    parsed.entities.insert(id, entity);
                }
                None => parsed.skipped += 1,
            }
            continue;
        }
        // Anything else at this level is not something this reads. Step over
        // one statement rather than stopping: a file is worth reading for the
        // instances it does hold.
        if !reader.skip_statement() {
            break;
        }
    }
    Ok(parsed)
}

struct Reader<'bytes> {
    bytes: &'bytes [u8],
    at: usize,
    length: usize,
    names: Names<'bytes>,
}

impl<'bytes> Reader<'bytes> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_whitespace(&mut self) {
        while self.at < self.length {
            match self.bytes[self.at] {
                b' ' | b'\t' | b'\r' | b'\n' => self.at += 1,
                // `/* ... */`
                b'/' if self.bytes.get(self.at + 1) == Some(&b'*') => {
                    self.at += 2;
                    while self.at + 1 < self.length
                        && !(self.bytes[self.at] == b'*' && self.bytes[self.at + 1] == b'/')
                    {
                        self.at += 1;
                    }
                    self.at = (self.at + 2).min(self.length);
                }
                _ => break,
            }
        }
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        let end = self.at + keyword.len();
        if end <= self.length && self.bytes[self.at..end].eq_ignore_ascii_case(keyword.as_bytes()) {
            self.at = end;
            return true;
        }
        false
    }

    fn take_semicolon(&mut self) {
        self.skip_whitespace();
        if self.peek() == Some(b';') {
            self.at += 1;
        }
    }

    /// Step past one `...;`, respecting the strings that may hold a semicolon.
    fn skip_statement(&mut self) -> bool {
        while self.at < self.length {
            match self.bytes[self.at] {
                b';' => {
                    self.at += 1;
                    return true;
                }
                b'\'' => {
                    let _ = self.read_text();
                }
                _ => self.at += 1,
            }
        }
        false
    }

    fn read_header(&mut self, parsed: &mut Parsed) {
        loop {
            self.skip_whitespace();
            if self.at >= self.length || self.take_keyword("ENDSEC") {
                self.take_semicolon();
                return;
            }
            let Some(name) = self.read_name() else {
                if !self.skip_statement() {
                    return;
                }
                continue;
            };
            let value = self.read_value();
            self.take_semicolon();
            let arguments = value.as_ref().and_then(Value::as_list).unwrap_or_default();
            match name.as_str() {
                // FILE_NAME's sixth attribute is the originating system.
                "FILE_NAME" => {
                    parsed.application = arguments
                        .get(5)
                        .and_then(Value::as_text)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned);
                }
                "FILE_SCHEMA" => {
                    parsed.schema = arguments
                        .first()
                        .and_then(Value::as_list)
                        .and_then(<[Value]>::first)
                        .and_then(Value::as_text)
                        .map(str::to_owned);
                }
                _ => {}
            }
        }
    }

    fn read_instance(&mut self) -> Option<(u64, Entity)> {
        self.at += 1; // '#'
        let id = self.read_unsigned()?;
        self.skip_whitespace();
        if self.peek() != Some(b'=') {
            self.skip_statement();
            return None;
        }
        self.at += 1;
        self.skip_whitespace();
        let type_name = self.read_name()?;
        // A complex instance - `#5=(A(..)B(..))` - is a mapped-type entity this
        // does not read. It is skipped whole rather than half-read.
        let Some(Value::List(attributes)) = self.read_value() else {
            self.skip_statement();
            return None;
        };
        self.take_semicolon();
        Some((
            id,
            Entity {
                type_name,
                attributes,
            },
        ))
    }

    fn read_name(&mut self) -> Option<TypeName> {
        let raw = self.read_name_bytes()?;
        Some(self.names.intern(raw))
    }

    /// The bytes of a name, still borrowed from the source.
    fn read_name_bytes(&mut self) -> Option<&'bytes [u8]> {
        self.skip_whitespace();
        let start = self.at;
        while self.at < self.length
            && (self.bytes[self.at].is_ascii_alphanumeric()
                || self.bytes[self.at] == b'_'
                || self.bytes[self.at] == b'-')
        {
            self.at += 1;
        }
        (self.at > start).then(|| &self.bytes[start..self.at])
    }

    fn read_unsigned(&mut self) -> Option<u64> {
        let start = self.at;
        while self.at < self.length && self.bytes[self.at].is_ascii_digit() {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()?
            .parse()
            .ok()
    }

    fn read_value(&mut self) -> Option<Value> {
        self.skip_whitespace();
        match self.peek()? {
            b'(' => {
                self.at += 1;
                let mut values = Vec::new();
                loop {
                    self.skip_whitespace();
                    match self.peek() {
                        Some(b')') => {
                            self.at += 1;
                            break;
                        }
                        Some(b',') => {
                            self.at += 1;
                        }
                        None => break,
                        _ => match self.read_value() {
                            Some(value) => values.push(value),
                            None => break,
                        },
                    }
                }
                Some(Value::List(values.into_boxed_slice()))
            }
            b'#' => {
                self.at += 1;
                self.read_unsigned().map(Value::Reference)
            }
            b'\'' => Some(Value::Text(self.read_text().into_boxed_str())),
            b'$' => {
                self.at += 1;
                Some(Value::Unset)
            }
            b'*' => {
                self.at += 1;
                Some(Value::Derived)
            }
            b'.' => {
                self.at += 1;
                let start = self.at;
                while self.at < self.length && self.bytes[self.at] != b'.' {
                    self.at += 1;
                }
                let raw = &self.bytes[start..self.at];
                self.at = (self.at + 1).min(self.length);
                Some(Value::Enumeration(self.names.intern_exact(raw)))
            }
            byte if byte.is_ascii_digit() || byte == b'-' || byte == b'+' => self.read_number(),
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let name = self.read_name()?;
                self.skip_whitespace();
                if self.peek() == Some(b'(') {
                    let inner = self.read_value()?;
                    // A one-element list is how `IFCLENGTHMEASURE(3.)` parses;
                    // unwrap it so callers read a number, not a list of one.
                    let inner = match inner {
                        Value::List(values) if values.len() == 1 => values.into_vec().remove(0),
                        other => other,
                    };
                    Some(Value::Typed(name, Box::new(inner)))
                } else {
                    Some(Value::Enumeration(name))
                }
            }
            _ => {
                self.at += 1;
                None
            }
        }
    }

    fn read_number(&mut self) -> Option<Value> {
        let start = self.at;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.at += 1;
        }
        let mut real = false;
        while self.at < self.length {
            match self.bytes[self.at] {
                byte if byte.is_ascii_digit() => self.at += 1,
                b'.' => {
                    real = true;
                    self.at += 1;
                }
                b'e' | b'E' => {
                    real = true;
                    self.at += 1;
                    if matches!(self.peek(), Some(b'-' | b'+')) {
                        self.at += 1;
                    }
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).ok()?;
        if real {
            // `3.` is how STEP writes three, which Rust parses.
            text.parse().ok().map(Value::Real)
        } else {
            text.parse().ok().map(Value::Integer)
        }
    }

    /// A quoted string, with the two escapes a file actually uses: `''` for a
    /// quote, and `\X2\....\X0\` for characters outside the base alphabet.
    fn read_text(&mut self) -> String {
        self.at += 1; // opening quote
        let mut out = String::new();
        while self.at < self.length {
            match self.bytes[self.at] {
                b'\'' => {
                    if self.bytes.get(self.at + 1) == Some(&b'\'') {
                        out.push('\'');
                        self.at += 2;
                    } else {
                        self.at += 1;
                        return decode_escapes(&out);
                    }
                }
                byte => {
                    out.push(byte as char);
                    self.at += 1;
                }
            }
        }
        decode_escapes(&out)
    }
}

/// Undo the ISO 10303-21 encodings for characters outside the base alphabet.
/// `\X2\0410...\X0\` is a run of UTF-16 code units; `\S\A` is the character
/// 128 above `A`; `\X\41` is one byte.
fn decode_escapes(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' {
            out.push(bytes[at] as char);
            at += 1;
            continue;
        }
        let rest = &text[at..];
        if let Some(body) = rest.strip_prefix("\\X2\\") {
            let end = body.find("\\X0\\").unwrap_or(body.len());
            let digits = &body[..end];
            let mut units = Vec::with_capacity(digits.len() / 4);
            for unit in digits.as_bytes().chunks(4) {
                if let Ok(text) = std::str::from_utf8(unit) {
                    if let Ok(value) = u16::from_str_radix(text, 16) {
                        units.push(value);
                    }
                }
            }
            out.push_str(&String::from_utf16_lossy(&units));
            at += 4 + end + if end < body.len() { 4 } else { 0 };
        } else if let Some(body) = rest.strip_prefix("\\S\\") {
            if let Some(character) = body.bytes().next() {
                out.push(char::from(character.wrapping_add(128)));
            }
            at += 4;
        } else if let Some(body) = rest.strip_prefix("\\X\\") {
            if let Ok(value) = u8::from_str_radix(&body[..body.len().min(2)], 16) {
                out.push(char::from(value));
            }
            at += 5;
        } else {
            out.push('\\');
            at += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Value, decode_escapes, parse};

    const MINIMAL: &str = "ISO-10303-21;\n\
        HEADER;\n\
        FILE_DESCRIPTION(('ViewDefinition [ReferenceView]'),'2;1');\n\
        FILE_NAME('t.ifc','2026-01-01T00:00:00',(''),(''),'','Autodesk Revit','');\n\
        FILE_SCHEMA(('IFC4'));\n\
        ENDSEC;\n\
        DATA;\n\
        #1=IFCCARTESIANPOINT((0.,1.5,-2.));\n\
        #2=IFCWALL('3vB2Y_$AH0Bhq',#1,$,*,.SOLIDWALL.,'It''s a wall');\n\
        ENDSEC;\n\
        END-ISO-10303-21;\n";

    /// The two types a nine-million-entity file holds millions of. Their size
    /// is the parse's memory, so it is pinned here rather than left to whoever
    /// next adds a variant: `Value` is one pointer-pair plus a tag, and a
    /// wider variant would cost this corpus hundreds of megabytes.
    #[test]
    fn the_hot_types_stay_narrow() {
        assert_eq!(std::mem::size_of::<super::Value>(), 24);
        assert_eq!(std::mem::size_of::<super::Entity>(), 24);
        assert_eq!(std::mem::size_of::<super::TypeName>(), 8);
    }

    #[test]
    fn reads_the_header_and_the_instances() {
        let parsed = parse(MINIMAL.as_bytes()).expect("a STEP file");
        assert_eq!(parsed.schema.as_deref(), Some("IFC4"));
        assert_eq!(parsed.application.as_deref(), Some("Autodesk Revit"));
        assert_eq!(parsed.entities.len(), 2);
        assert_eq!(parsed.skipped, 0);

        let point = parsed.get(1).expect("#1");
        assert_eq!(point.type_name, "IFCCARTESIANPOINT");
        let coordinates = point.attributes[0].as_list().expect("a list");
        assert_eq!(coordinates[0].as_number(), Some(0.0));
        assert_eq!(coordinates[1].as_number(), Some(1.5));
        assert_eq!(coordinates[2].as_number(), Some(-2.0));
    }

    #[test]
    fn reads_the_values_a_file_actually_writes() {
        let parsed = parse(MINIMAL.as_bytes()).expect("a STEP file");
        let wall = parsed.get(2).expect("#2");
        assert_eq!(wall.attributes[0].as_text(), Some("3vB2Y_$AH0Bhq"));
        assert_eq!(wall.attributes[1].as_reference(), Some(1));
        // `$` and `*` are absences, and `attribute` reports them as such.
        assert_eq!(wall.attributes[2], Value::Unset);
        assert_eq!(wall.attributes[3], Value::Derived);
        assert!(wall.attribute(2).is_none());
        assert!(wall.attribute(3).is_none());
        assert_eq!(wall.attributes[4].as_enumeration(), Some("SOLIDWALL"));
        // A doubled quote is one quote, not the end of the string.
        assert_eq!(wall.attributes[5].as_text(), Some("It's a wall"));
    }

    #[test]
    fn decodes_the_escapes_a_cyrillic_name_arrives_in() {
        assert_eq!(decode_escapes("\\X2\\041E043D\\X0\\"), "Он");
        assert_eq!(decode_escapes("plain"), "plain");
        // An unterminated run is still read rather than dropped.
        assert_eq!(decode_escapes("\\X2\\041E"), "О");
    }

    #[test]
    fn refuses_something_that_is_not_a_step_file() {
        assert!(parse(b"<html>").is_err());
    }

    #[test]
    fn counts_an_instance_it_cannot_read_rather_than_guessing() {
        let text = "ISO-10303-21;\nDATA;\n#1=IFCWALL('a');\n\
                    #2=(IFCA(1)IFCB(2));\n#3=IFCSLAB('c');\nENDSEC;\nEND-ISO-10303-21;\n";
        let parsed = parse(text.as_bytes()).expect("a STEP file");
        assert_eq!(parsed.entities.len(), 2);
        assert_eq!(parsed.skipped, 1);
        assert!(parsed.get(3).is_some(), "parsing continued past the skip");
    }
}

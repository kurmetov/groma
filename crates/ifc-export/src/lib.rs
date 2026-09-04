#![forbid(unsafe_code)]

use std::io::{self, Write};

mod mapping;
mod metadata;

pub use mapping::element_type_for_source;
pub use metadata::{MetadataError, MetadataOptions, metadata_ifc};

const IFC64: &[u8; 64] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";

/// IFC's fixed-width, 22-character encoding of a 128-bit UUID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IfcGuid([u8; 22]);

impl IfcGuid {
    /// Encode UUID bytes in network order.
    #[must_use]
    pub fn from_uuid_bytes(bytes: [u8; 16]) -> Self {
        let mut value = u128::from_be_bytes(bytes);
        let mut encoded = [b'0'; 22];
        for character in encoded.iter_mut().rev() {
            *character = IFC64[(value & 0x3f) as usize];
            value >>= 6;
        }
        Self(encoded)
    }

    /// Create a deterministic RFC 4122 version-5 UUID, then encode it as an
    /// IFC `GlobalId`. The namespace should identify the source model; `name`
    /// should identify the entity within that model.
    #[must_use]
    pub fn from_namespace_and_name(namespace: [u8; 16], name: &[u8]) -> Self {
        Self::from_uuid_bytes(uuid_v5(namespace, name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        // The constructor writes only bytes from the ASCII IFC64 alphabet.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

/// Derive deterministic RFC 4122 version-5 UUID bytes.
#[must_use]
pub fn uuid_v5(namespace: [u8; 16], name: &[u8]) -> [u8; 16] {
    let mut sha1 = Sha1::default();
    sha1.update(&namespace);
    sha1.update(name);
    let digest = sha1.finish();
    let mut uuid = [0_u8; 16];
    uuid.copy_from_slice(&digest[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x50;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    uuid
}

impl std::fmt::Display for IfcGuid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EntityRef(u64);

impl EntityRef {
    #[must_use]
    pub const fn number(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StepValue {
    Omitted,
    Derived,
    Integer(i64),
    Real(f64),
    Boolean(bool),
    String(String),
    Enumeration(String),
    Reference(EntityRef),
    List(Vec<Self>),
    Typed { name: String, value: Box<Self> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepHeader {
    pub file_name: String,
    pub timestamp: String,
    pub authors: Vec<String>,
    pub organizations: Vec<String>,
    pub preprocessor_version: String,
    pub originating_system: String,
    pub authorization: String,
    pub schema: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Entity {
    name: String,
    arguments: Vec<StepValue>,
}

/// Small, deterministic ISO 10303-21 emitter. It deliberately models syntax,
/// not IFC semantics; higher layers remain responsible for entity signatures.
#[derive(Clone, Debug, PartialEq)]
pub struct StepFile {
    header: StepHeader,
    entities: Vec<Entity>,
}

impl StepFile {
    #[must_use]
    pub const fn new(header: StepHeader) -> Self {
        Self {
            header,
            entities: Vec::new(),
        }
    }

    /// Append one entity and return its one-based STEP reference.
    ///
    /// # Panics
    ///
    /// Panics when `name` is not an uppercase EXPRESS identifier.
    pub fn push(&mut self, name: impl Into<String>, arguments: Vec<StepValue>) -> EntityRef {
        let name = name.into();
        assert!(is_express_identifier(&name), "invalid EXPRESS identifier");
        self.entities.push(Entity { name, arguments });
        EntityRef(self.entities.len() as u64)
    }

    /// Write a complete exchange file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error from the destination or `InvalidInput` for a
    /// non-finite real value, an invalid typed/enumeration identifier, or an
    /// empty required author/organization list.
    pub fn write_to(&self, mut writer: impl Write) -> io::Result<()> {
        if self.header.authors.is_empty() || self.header.organizations.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "STEP FILE_NAME requires at least one author and organization",
            ));
        }
        writeln!(writer, "ISO-10303-21;")?;
        writeln!(writer, "HEADER;")?;
        writeln!(
            writer,
            "FILE_DESCRIPTION(('ViewDefinition [ReferenceView_V1.2]'),'2;1');"
        )?;
        write!(writer, "FILE_NAME(")?;
        write_step_string(&mut writer, &self.header.file_name)?;
        write!(writer, ",")?;
        write_step_string(&mut writer, &self.header.timestamp)?;
        write!(writer, ",")?;
        write_string_list(&mut writer, &self.header.authors)?;
        write!(writer, ",")?;
        write_string_list(&mut writer, &self.header.organizations)?;
        for value in [
            &self.header.preprocessor_version,
            &self.header.originating_system,
            &self.header.authorization,
        ] {
            write!(writer, ",")?;
            write_step_string(&mut writer, value)?;
        }
        writeln!(writer, ");")?;
        write!(writer, "FILE_SCHEMA((")?;
        write_step_string(&mut writer, &self.header.schema)?;
        writeln!(writer, "));")?;
        writeln!(writer, "ENDSEC;")?;
        writeln!(writer, "DATA;")?;
        for (index, entity) in self.entities.iter().enumerate() {
            write!(writer, "#{}={}(", index + 1, entity.name)?;
            write_values(&mut writer, &entity.arguments)?;
            writeln!(writer, ");")?;
        }
        writeln!(writer, "ENDSEC;")?;
        writeln!(writer, "END-ISO-10303-21;")
    }
}

fn is_express_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_' || byte.is_ascii_digit())
        && value.as_bytes()[0].is_ascii_uppercase()
}

fn write_values(writer: &mut impl Write, values: &[StepValue]) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            write!(writer, ",")?;
        }
        write_value(writer, value)?;
    }
    Ok(())
}

fn write_value(writer: &mut impl Write, value: &StepValue) -> io::Result<()> {
    match value {
        StepValue::Omitted => write!(writer, "$"),
        StepValue::Derived => write!(writer, "*"),
        StepValue::Integer(value) => write!(writer, "{value}"),
        StepValue::Real(value) if value.is_finite() => write!(writer, "{value:.15e}"),
        StepValue::Real(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "STEP real value must be finite",
        )),
        StepValue::Boolean(value) => write!(writer, ".{}.", if *value { "T" } else { "F" }),
        StepValue::String(value) => write_step_string(writer, value),
        StepValue::Enumeration(value) => {
            if !is_express_identifier(value) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid STEP enumeration",
                ));
            }
            write!(writer, ".{value}.")
        }
        StepValue::Reference(reference) => write!(writer, "#{}", reference.0),
        StepValue::List(values) => {
            write!(writer, "(")?;
            write_values(writer, values)?;
            write!(writer, ")")
        }
        StepValue::Typed { name, value } => {
            if !is_express_identifier(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid STEP typed-value name",
                ));
            }
            write!(writer, "{name}(")?;
            write_value(writer, value)?;
            write!(writer, ")")
        }
    }
}

fn write_string_list(writer: &mut impl Write, values: &[String]) -> io::Result<()> {
    write!(writer, "(")?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            write!(writer, ",")?;
        }
        write_step_string(writer, value)?;
    }
    write!(writer, ")")
}

fn write_step_string(writer: &mut impl Write, value: &str) -> io::Result<()> {
    write!(writer, "'")?;
    if value
        .chars()
        .all(|character| character.is_ascii_graphic() || character == ' ')
    {
        for character in value.chars() {
            match character {
                '\'' => write!(writer, "''")?,
                '\\' => write!(writer, "\\\\")?,
                other => write!(writer, "{other}")?,
            }
        }
    } else {
        write!(writer, "\\X2\\")?;
        for unit in value.encode_utf16() {
            write!(writer, "{unit:04X}")?;
        }
        write!(writer, "\\X0\\")?;
    }
    write!(writer, "'")
}

#[derive(Default)]
struct Sha1 {
    state: [u32; 5],
    bytes: Vec<u8>,
}

impl Sha1 {
    fn update(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn finish(mut self) -> [u8; 20] {
        let bit_length = (self.bytes.len() as u64).wrapping_mul(8);
        self.bytes.push(0x80);
        while self.bytes.len() % 64 != 56 {
            self.bytes.push(0);
        }
        self.bytes.extend_from_slice(&bit_length.to_be_bytes());
        self.state = [
            0x6745_2301,
            0xefcd_ab89,
            0x98ba_dcfe,
            0x1032_5476,
            0xc3d2_e1f0,
        ];
        for chunk in self.bytes.chunks_exact(64) {
            sha1_compress(&mut self.state, chunk);
        }
        let mut digest = [0_u8; 20];
        for (target, value) in digest.chunks_exact_mut(4).zip(self.state) {
            target.copy_from_slice(&value.to_be_bytes());
        }
        digest
    }
}

#[allow(clippy::many_single_char_names)] // Names follow the SHA-1 specification.
fn sha1_compress(state: &mut [u32; 5], chunk: &[u8]) {
    let mut words = [0_u32; 80];
    for (target, source) in words[..16].iter_mut().zip(chunk.chunks_exact(4)) {
        *target = u32::from_be_bytes(source.try_into().expect("four-byte chunk"));
    }
    for index in 16..80 {
        words[index] =
            (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                .rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (index, word) in words.into_iter().enumerate() {
        let (function, constant) = match index {
            0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let next = a
            .rotate_left(5)
            .wrapping_add(function)
            .wrapping_add(e)
            .wrapping_add(constant)
            .wrapping_add(word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = next;
    }
    for (target, value) in state.iter_mut().zip([a, b, c, d, e]) {
        *target = target.wrapping_add(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_uuid_to_the_ifc_alphabet() {
        let bytes = u128::from_str_radix("00112233445566778899aabbccddeeff", 16)
            .unwrap()
            .to_be_bytes();
        assert_eq!(
            IfcGuid::from_uuid_bytes(bytes).as_str(),
            "004I8pH5LcTuYPghlCtUx$"
        );
    }

    #[test]
    fn creates_an_rfc_4122_v5_identity() {
        let namespace = u128::from_str_radix("6ba7b8109dad11d180b400c04fd430c8", 16)
            .unwrap()
            .to_be_bytes();
        let guid = IfcGuid::from_namespace_and_name(namespace, b"www.widgets.com");
        assert_eq!(guid.as_str(), "0Xz$ZUW55RYOQ00PNlUOjg");
    }

    #[test]
    fn emits_header_entities_references_and_unicode() {
        let header = StepHeader {
            file_name: "model.ifc".to_owned(),
            timestamp: "2026-09-04T12:00:00+06:00".to_owned(),
            authors: vec!["Rivet".to_owned()],
            organizations: vec!["Rivet".to_owned()],
            preprocessor_version: "Rivet 0.1.0".to_owned(),
            originating_system: "Rivet".to_owned(),
            authorization: String::new(),
            schema: "IFC4".to_owned(),
        };
        let mut file = StepFile::new(header);
        let point = file.push(
            "IFCCARTESIANPOINT",
            vec![StepValue::List(vec![
                StepValue::Real(0.0),
                StepValue::Real(0.0),
                StepValue::Real(0.0),
            ])],
        );
        file.push(
            "IFCAXIS2PLACEMENT3D",
            vec![
                StepValue::Reference(point),
                StepValue::Omitted,
                StepValue::String("Этаж '1'".to_owned()),
            ],
        );
        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("ISO-10303-21;\nHEADER;"));
        assert!(text.contains("#1=IFCCARTESIANPOINT((0.000000000000000e0"));
        assert!(text.contains("#2=IFCAXIS2PLACEMENT3D(#1,$,'\\X2\\"));
        assert!(text.ends_with("END-ISO-10303-21;\n"));
    }

    #[test]
    fn rejects_non_finite_real_values() {
        let mut bytes = Vec::new();
        assert!(write_value(&mut bytes, &StepValue::Real(f64::NAN)).is_err());
    }

    #[test]
    fn rejects_an_empty_header_organization() {
        let file = StepFile::new(StepHeader {
            file_name: "model.ifc".to_owned(),
            timestamp: "2026-09-04T12:00:00Z".to_owned(),
            authors: vec!["Rivet".to_owned()],
            organizations: Vec::new(),
            preprocessor_version: "Rivet 0.1.0".to_owned(),
            originating_system: "Rivet".to_owned(),
            authorization: String::new(),
            schema: "IFC4".to_owned(),
        });
        assert_eq!(
            file.write_to(Vec::new()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

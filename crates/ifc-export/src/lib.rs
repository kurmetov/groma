#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    io::{self, Write},
};

mod class_mapping;
pub mod extrusion;
mod ifc4_entities;
mod metadata;
mod quantities;
mod settings;

pub use class_mapping::{ClassMapping, ClassMappingError, Mapped};
// Both tables live in `bim-convert`, which the viewer scene and the RVT
// reader also ask. Re-exported so that this crate's published surface - the
// one mapping `export-ifc` applies - is unchanged by where it is kept.
pub use bim_convert::{element_type_for_source, ifc_entity_name};
pub use metadata::{MetadataError, MetadataOptions, metadata_ifc, metadata_ifc_reported};
pub use settings::{
    ExportSettings, LengthUnit, ProjectSettings, PropertySetSettings, SettingsError, ViewDefinition,
};

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
    /// The `FILE_DESCRIPTION` strings, in the order they are written. IFC puts
    /// the model view definition first; a writer may state more beside it, as
    /// Revit does with the exchange requirement and coordinate reference.
    pub description: Vec<String>,
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

/// The entity types written once per distinct value.
///
/// IFC gives an instance of these no identity beyond what it states: two
/// `IfcCartesianPoint`s with the same coordinates are the same point, and a
/// reader that follows a reference to either arrives at the same thing. So one
/// is written and referenced wherever that value recurs.
///
/// Everything else keeps one instance per use even where two would read alike.
/// A product, a representation, a property set, a relationship, a face, a loop
/// and a shell all stay one per use, because there the instance *is* the thing
/// rather than the value, and a reader counting them would otherwise get a
/// different answer.
///
/// The list is deliberately short of the topology above an edge. Sharing a
/// face or a shell between two products is legal and would collapse repeated
/// families further, but it also makes one solid's boundary another's, which
/// is a claim about the model rather than about how it is written down.
///
/// `IfcPolyline` is on the list and has the one exception to it. This exporter
/// writes one both as an edge's geometry, where it is a curve like any other,
/// and as the sole item of an `Axis` representation, where IFC intends an item
/// to belong to the one representation holding it. The second is written
/// through [`StepFile::push_once`], so two pipes with the same local
/// centreline keep an item each.
const SHARED_BY_VALUE: &[&str] = &[
    "IFCAXIS1PLACEMENT",
    "IFCAXIS2PLACEMENT2D",
    "IFCAXIS2PLACEMENT3D",
    "IFCCARTESIANPOINT",
    // The identity, which every mapped body is placed by: the element's own
    // placement is what stands it where it stands, so the operator states
    // nothing but where the body's own origin is. One per file rather than
    // one per element that carries a body.
    "IFCCARTESIANTRANSFORMATIONOPERATOR3D",
    "IFCCIRCLE",
    "IFCCYLINDRICALSURFACE",
    "IFCDIRECTION",
    "IFCEDGECURVE",
    "IFCLINE",
    "IFCPLANE",
    "IFCPOLYLINE",
    "IFCVECTOR",
    "IFCVERTEXPOINT",
];

/// Small, deterministic ISO 10303-21 emitter. It deliberately models syntax,
/// not IFC semantics; higher layers remain responsible for entity signatures.
#[derive(Clone, Debug)]
pub struct StepFile {
    header: StepHeader,
    entities: Vec<Entity>,
    /// Where a value in [`SHARED_BY_VALUE`] was already written, by the text
    /// the file states it as. Keying on that text is what makes the test
    /// exact: two arguments are the same value when the file would say them
    /// the same way, which is the only equality the reader can see.
    shared: HashMap<Box<[u8]>, EntityRef>,
}

impl StepFile {
    #[must_use]
    pub fn new(header: StepHeader) -> Self {
        Self {
            header,
            entities: Vec::new(),
            shared: HashMap::new(),
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
        if SHARED_BY_VALUE.contains(&name.as_str()) {
            let mut key = Vec::new();
            key.extend_from_slice(name.as_bytes());
            // A value `write_to` would refuse - a real that is not finite - is
            // not shared, so that it is still written and still reported.
            if write_values(&mut key, &arguments).is_ok() {
                if let Some(&already) = self.shared.get(key.as_slice()) {
                    return already;
                }
                let written = self.append(name, arguments);
                self.shared.insert(key.into_boxed_slice(), written);
                return written;
            }
        }
        self.append(name, arguments)
    }

    /// Append one entity that is never shared, whatever its name.
    ///
    /// For the one place a type on [`SHARED_BY_VALUE`] is written as something
    /// with an identity of its own rather than as a value: an `IfcPolyline`
    /// that is a representation's item rather than an edge's geometry.
    ///
    /// # Panics
    ///
    /// Panics when `name` is not an uppercase EXPRESS identifier.
    pub fn push_once(&mut self, name: impl Into<String>, arguments: Vec<StepValue>) -> EntityRef {
        let name = name.into();
        assert!(is_express_identifier(&name), "invalid EXPRESS identifier");
        self.append(name, arguments)
    }

    fn append(&mut self, name: String, arguments: Vec<StepValue>) -> EntityRef {
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
        write!(writer, "FILE_DESCRIPTION(")?;
        write_string_list(&mut writer, &self.header.description)?;
        writeln!(writer, ",'2;1');")?;
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
        StepValue::Real(value) if value.is_finite() => write_real(writer, *value),
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

/// How many significant digits of a real the file states.
///
/// Twelve, and the twelfth is where the measurement says to stop. A corner
/// this exporter states twice - once for each solid that meets there - comes
/// out of two different chains of matrix multiplications, so the two doubles
/// differ in their last few bits although the model has one corner. Rounding
/// the 650 562 coordinates of the SMALL export to a given number of digits and
/// counting the distinct points left:
///
/// | digits | distinct points | of the bytes their digits took |
/// | ---: | ---: | ---: |
/// | 16 | 213 933 | 97.5% |
/// | 15 | 194 430 | 91.6% |
/// | 14 | 139 639 | 83.2% |
/// | 13 | 70 317 | 70.9% |
/// | **12** | **49 868** | **61.5%** |
/// | 11 | 46 811 | 57.1% |
/// | 10 | 46 203 | 54.0% |
/// | 9 | 46 017 | 50.7% |
///
/// The count falls by four fifths down to twelve digits and then stops
/// falling: past that there is no more arithmetic noise to merge, and what
/// would merge next are points the model really does state apart. Twelve is
/// therefore where noise ends rather than a tolerance anyone chose, and it is
/// six orders of magnitude finer than the `1e-5` metre precision the file's
/// own `IfcGeometricRepresentationContext` declares.
///
/// This is the one thing in the emitter that does not write back exactly what
/// it was given, and it is stated here rather than hidden in a formatter.
const SIGNIFICANT_DIGITS: usize = 12;

/// Write a real the way STEP asks for one, in the fewest digits that read back
/// as the same value to [`SIGNIFICANT_DIGITS`].
///
/// The emitter wrote fifteen fractional digits in exponential form before
/// this, which spends nineteen bytes stating zero. ISO 10303-21 requires the
/// decimal point, which is why a whole number keeps a trailing one.
pub(crate) fn write_real(writer: &mut impl Write, value: f64) -> io::Result<()> {
    // `Display` never uses an exponent, which is what a coordinate wants and
    // ruinous outside that range: 1e-300 would be three hundred zeroes. So the
    // range where the plain form is the short one picks it, and `LowerExp`
    // - also shortest-round-trip - takes everything else.
    //
    // Negative zero is the same point as zero and is written as one, so that
    // the two share an entity rather than reading as two.
    if value == 0.0 {
        return writer.write_all(b"0.");
    }
    // Rounded by writing the digits that are kept and reading them back, which
    // is exact where scaling by a power of ten is not: `3300.000000000003`
    // becomes `3300.` rather than `3299.9999999999995`.
    let mut rounding = [0_u8; 32];
    let mut digits = io::Cursor::new(&mut rounding[..]);
    write!(digits, "{value:.*E}", SIGNIFICANT_DIGITS - 1)?;
    let kept = usize::try_from(digits.position()).unwrap_or(0);
    let value = std::str::from_utf8(&rounding[..kept])
        .ok()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|rounded| rounded.is_finite())
        .unwrap_or(value);
    if value == 0.0 {
        return writer.write_all(b"0.");
    }
    let mut buffer = [0_u8; 64];
    let mut at = io::Cursor::new(&mut buffer[..]);
    if (1e-4..1e15).contains(&value.abs()) {
        write!(at, "{value}")?;
    } else {
        write!(at, "{value:E}")?;
    }
    let written = usize::try_from(at.position()).unwrap_or(0);
    let text = &buffer[..written];
    // Both forms can come back without a point - `12`, or `1E300` - and
    // neither is a STEP real until it has one.
    match text.iter().position(|byte| *byte == b'E') {
        Some(exponent) if !text[..exponent].contains(&b'.') => {
            writer.write_all(&text[..exponent])?;
            writer.write_all(b".")?;
            writer.write_all(&text[exponent..])
        }
        Some(_) => writer.write_all(text),
        None if text.contains(&b'.') => writer.write_all(text),
        None => {
            writer.write_all(text)?;
            writer.write_all(b".")
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

    fn header() -> StepHeader {
        StepHeader {
            description: vec!["ViewDefinition [DesignTransferView_V1.0]".to_owned()],
            file_name: "model.ifc".to_owned(),
            timestamp: "2026-09-04T12:00:00+06:00".to_owned(),
            authors: vec!["groma".to_owned()],
            organizations: vec!["groma".to_owned()],
            preprocessor_version: "groma 0.1.0".to_owned(),
            originating_system: "groma".to_owned(),
            authorization: String::new(),
            schema: "IFC4".to_owned(),
        }
    }

    #[test]
    fn emits_header_entities_references_and_unicode() {
        let mut file = StepFile::new(header());
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
        assert!(
            text.contains("FILE_DESCRIPTION(('ViewDefinition [DesignTransferView_V1.0]'),'2;1');")
        );
        assert!(text.contains("#1=IFCCARTESIANPOINT((0.,0.,0.));"));
        assert!(text.contains("#2=IFCAXIS2PLACEMENT3D(#1,$,'\\X2\\"));
        assert!(text.ends_with("END-ISO-10303-21;\n"));
    }

    /// Every real the file states must read back as the value it was written
    /// from, to the digits [`SIGNIFICANT_DIGITS`] keeps - that is the whole
    /// licence for the short form and for the rounding.
    #[test]
    fn writes_the_shortest_real_that_reads_back_the_same() {
        for value in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.1,
            1.0 / 3.0,
            3.048,
            3_300.000_000_000_003,
            1e-5,
            1e15,
            1e300,
            -1e-300,
            f64::MIN_POSITIVE,
            f64::MAX,
            std::f64::consts::PI,
        ] {
            let mut bytes = Vec::new();
            write_real(&mut bytes, value).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(
                text.contains('.'),
                "{value} wrote {text}, which is not a STEP real"
            );
            // `0.` and `1.E300` are STEP reals and not Rust literals; the
            // point is there for the schema and says nothing about the value.
            let literal = text.replace(".E", ".0E");
            let literal = literal
                .strip_suffix('.')
                .map_or(literal.clone(), |head| format!("{head}.0"));
            let read: f64 = literal.parse().expect("a number");
            let expected: f64 = format!("{value:.*E}", SIGNIFICANT_DIGITS - 1)
                .parse()
                .expect("a number");
            // Exact equality is the claim: both sides are the same decimal
            // text parsed by the same parser, so anything but equality means
            // the shortest form lost a digit the rounding kept.
            #[allow(clippy::float_cmp)]
            {
                assert_eq!(read, expected, "{value} wrote {text}");
            }
        }
        let mut bytes = Vec::new();
        write_real(&mut bytes, -0.0).unwrap();
        assert_eq!(bytes, b"0.", "negative zero is the same point as zero");
    }

    /// The rounding is what collapses a corner two chains of matrix
    /// multiplications state slightly differently into the one point the model
    /// has. It must reach the writer, and it must reach the sharing.
    #[test]
    fn states_a_coordinate_without_its_arithmetic_noise() {
        let mut bytes = Vec::new();
        write_real(&mut bytes, 3_300.000_000_000_003).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "3300.");

        let mut file = StepFile::new(header());
        let point = |file: &mut StepFile, z: f64| {
            file.push(
                "IFCCARTESIANPOINT",
                vec![StepValue::List(vec![
                    StepValue::Real(0.0),
                    StepValue::Real(0.0),
                    StepValue::Real(z),
                ])],
            )
        };
        assert_eq!(
            point(&mut file, 3.3),
            point(&mut file, 3.300_000_000_000_000_3),
            "one corner, however the arithmetic reached it"
        );
        // Twelve digits apart is a distinction the model states, not noise.
        assert_ne!(point(&mut file, 3.3), point(&mut file, 3.300_000_000_1));
    }

    /// A resource with no identity beyond its value is written once. Anything
    /// a reader can count as an object of its own is not.
    #[test]
    fn shares_a_value_and_never_an_object() {
        let mut file = StepFile::new(header());
        let point = |file: &mut StepFile| {
            file.push(
                "IFCCARTESIANPOINT",
                vec![StepValue::List(vec![
                    StepValue::Real(1.5),
                    StepValue::Real(0.0),
                    StepValue::Real(0.0),
                ])],
            )
        };
        let first = point(&mut file);
        let second = point(&mut file);
        assert_eq!(first, second, "one point, referenced twice");

        let elsewhere = file.push(
            "IFCCARTESIANPOINT",
            vec![StepValue::List(vec![
                StepValue::Real(1.5),
                StepValue::Real(0.0),
                StepValue::Real(1.0),
            ])],
        );
        assert_ne!(first, elsewhere, "a different point is a different entity");

        let wall =
            |file: &mut StepFile| file.push("IFCWALL", vec![StepValue::String("W1".to_owned())]);
        assert_ne!(
            wall(&mut file),
            wall(&mut file),
            "two walls that read alike are still two walls"
        );

        let mut bytes = Vec::new();
        file.write_to(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("=IFCCARTESIANPOINT((1.5,0.,0.));").count(), 1);
        assert_eq!(text.matches("=IFCWALL(").count(), 2);
    }

    #[test]
    fn rejects_non_finite_real_values() {
        let mut bytes = Vec::new();
        assert!(write_value(&mut bytes, &StepValue::Real(f64::NAN)).is_err());
    }

    #[test]
    fn rejects_an_empty_header_organization() {
        let file = StepFile::new(StepHeader {
            description: Vec::new(),
            file_name: "model.ifc".to_owned(),
            timestamp: "2026-09-04T12:00:00Z".to_owned(),
            authors: vec!["groma".to_owned()],
            organizations: Vec::new(),
            preprocessor_version: "groma 0.1.0".to_owned(),
            originating_system: "groma".to_owned(),
            authorization: String::new(),
            schema: "IFC4".to_owned(),
        });
        assert_eq!(
            file.write_to(Vec::new()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

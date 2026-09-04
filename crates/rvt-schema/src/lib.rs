#![forbid(unsafe_code)]

use std::{error::Error, fmt};

/// First class index assigned by Revit's schema reader. Lower values are
/// reserved for built-in types that do not have definitions in this stream.
pub const INITIAL_CLASS_INDEX: u16 = 12;

const TERMINATOR_LEN: usize = 8;
const MAX_NESTING_DEPTH: usize = 64;
const MAX_CLASSES: usize = 0x8000 - INITIAL_CLASS_INDEX as usize;
const MAX_PROPERTIES: usize = 1 << 21;
const MIN_PROPERTY_LEN: usize = 8;
const GUID_LEN: usize = 16;

/// Options that cannot be inferred from the bytes in `Formats/Latest`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaReadOptions {
    /// Container format version. Versions above two include a GUID table at
    /// the end of every class definition.
    pub stream_version: u32,
}

impl Default for SchemaReadOptions {
    fn default() -> Self {
        Self { stream_version: 3 }
    }
}

/// A complete, strictly tiled `Formats/Latest` payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Schema {
    /// Every class in allocation order, including inline definitions.
    pub classes: Vec<ClassDefinition>,
    /// Definitions written at stream level rather than inside a type reference.
    pub top_level_class_count: usize,
    /// Sum of property counts declared directly by all classes.
    pub property_count: usize,
    /// Property records actually parsed, including nested tuple descriptors.
    pub parsed_property_count: usize,
    /// Bytes occupied by class definitions before the zero terminator.
    pub consumed_bytes: usize,
    /// Bytes following the verified eight-byte terminator, preserved verbatim.
    pub trailing_bytes: Vec<u8>,
    pub unresolved_references: Vec<UnresolvedReference>,
    pub inline_index_mismatches: Vec<InlineIndexMismatch>,
}

impl Schema {
    /// Parse a modern schema stream (stream format version 3).
    ///
    /// # Errors
    ///
    /// Returns an error rather than a partial schema if records do not tile up
    /// to the required eight-byte terminator.
    pub fn parse(data: &[u8]) -> Result<Self, SchemaError> {
        Self::parse_with_options(data, SchemaReadOptions::default())
    }

    /// Parse a schema stream using an explicit container format version.
    ///
    /// # Errors
    ///
    /// Returns an error for truncation, invalid discriminators, unsafe counts,
    /// excessive nesting, or a missing terminator.
    pub fn parse_with_options(
        data: &[u8],
        options: SchemaReadOptions,
    ) -> Result<Self, SchemaError> {
        Reader::new(data, options).read_schema()
    }

    #[must_use]
    pub fn class_by_index(&self, index: u16) -> Option<&ClassDefinition> {
        index
            .checked_sub(INITIAL_CLASS_INDEX)
            .and_then(|offset| self.classes.get(usize::from(offset)))
    }

    #[must_use]
    pub fn class_by_name(&self, name: &str) -> Option<&ClassDefinition> {
        self.classes.iter().find(|class| class.name == name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassDefinition {
    pub index: u16,
    pub name: String,
    /// Original single-byte name units. This remains lossless even if a future
    /// file uses bytes that are not UTF-8.
    pub name_bytes: Vec<u8>,
    pub parent: TypeReference,
    pub version: i32,
    pub properties: Vec<PropertyDefinition>,
    /// GUIDs in file byte order; byte-order semantics are intentionally not
    /// inferred here.
    pub guids: Vec<[u8; GUID_LEN]>,
    /// A class header word whose meaning is not established.
    pub unknown_word: i16,
    pub inline: bool,
    pub offset: usize,
    pub end_offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyDefinition {
    pub name: String,
    pub name_bytes: Vec<u8>,
    pub field_type: FieldType,
    /// Original packed byte from which `loading_mode` and `item_mode` derive.
    pub raw_modes: u8,
    pub loading_mode: u8,
    pub item_mode: i8,
    /// A property header word whose meaning is not established.
    pub unknown_word: i16,
    pub size: Option<i32>,
    pub element: Option<Box<Self>>,
    pub static_type: Option<TypeReference>,
    /// Extra word carried by an object property named exactly one space.
    pub space_name_word: Option<i16>,
    pub offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
    Bool,
    Integer8,
    Integer16,
    Integer32,
    Integer32Alternate,
    Float32,
    Float64,
    String,
    Guid,
    Integer16Alternate,
    Integer64,
    Tuple,
    Object,
}

impl FieldType {
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0x01 => Some(Self::Bool),
            0x02 => Some(Self::Integer8),
            0x03 => Some(Self::Integer16),
            0x04 => Some(Self::Integer32),
            0x05 => Some(Self::Integer32Alternate),
            0x06 => Some(Self::Float32),
            0x07 => Some(Self::Float64),
            0x08 => Some(Self::String),
            0x09 => Some(Self::Guid),
            0x0a => Some(Self::Integer16Alternate),
            0x0b => Some(Self::Integer64),
            0x0d => Some(Self::Tuple),
            0x0e => Some(Self::Object),
            _ => None,
        }
    }

    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Bool => 0x01,
            Self::Integer8 => 0x02,
            Self::Integer16 => 0x03,
            Self::Integer32 => 0x04,
            Self::Integer32Alternate => 0x05,
            Self::Float32 => 0x06,
            Self::Float64 => 0x07,
            Self::String => 0x08,
            Self::Guid => 0x09,
            Self::Integer16Alternate => 0x0a,
            Self::Integer64 => 0x0b,
            Self::Tuple => 0x0d,
            Self::Object => 0x0e,
        }
    }

    #[must_use]
    pub const fn fixed_width(self) -> Option<usize> {
        match self {
            Self::Bool | Self::Integer8 => Some(1),
            Self::Integer16 | Self::Integer16Alternate => Some(2),
            Self::Integer32 | Self::Integer32Alternate | Self::Float32 => Some(4),
            Self::Float64 | Self::Integer64 => Some(8),
            Self::Guid => Some(16),
            Self::String | Self::Tuple | Self::Object => None,
        }
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Bool => "bool",
            Self::Integer8 => "int8",
            Self::Integer16 | Self::Integer16Alternate => "int16",
            Self::Integer32 | Self::Integer32Alternate => "int32",
            Self::Float32 => "float32",
            Self::Float64 => "float64",
            Self::String => "string",
            Self::Guid => "guid",
            Self::Integer64 => "int64",
            Self::Tuple => "tuple",
            Self::Object => "object",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeReference {
    None,
    Inline {
        index: u16,
        name: String,
        declared_index: u16,
    },
    Reference {
        index: u16,
        name: String,
    },
    Unresolved {
        index: u16,
    },
}

impl TypeReference {
    #[must_use]
    pub const fn index(&self) -> Option<u16> {
        match self {
            Self::None => None,
            Self::Inline { index, .. }
            | Self::Reference { index, .. }
            | Self::Unresolved { index } => Some(*index),
        }
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Inline { name, .. } | Self::Reference { name, .. } => Some(name),
            Self::None | Self::Unresolved { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedReference {
    pub offset: usize,
    pub index: u16,
    pub defined_class_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InlineIndexMismatch {
    pub offset: usize,
    pub index: u16,
    pub declared_index: u16,
}

/// Failure diagnostics deliberately contain counts, not partial records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaError {
    message: String,
    offset: usize,
    classes_read: usize,
    properties_read: usize,
}

impl SchemaError {
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    #[must_use]
    pub const fn classes_read(&self) -> usize {
        self.classes_read
    }

    #[must_use]
    pub const fn properties_read(&self) -> usize {
        self.properties_read
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at byte {} ({} classes and {} properties read)",
            self.message, self.offset, self.classes_read, self.properties_read
        )
    }
}

impl Error for SchemaError {}

struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
    options: SchemaReadOptions,
    classes: Vec<Option<ClassDefinition>>,
    unresolved: Vec<UnresolvedReference>,
    mismatches: Vec<InlineIndexMismatch>,
    declared_properties: usize,
    read_properties: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], options: SchemaReadOptions) -> Self {
        Self {
            data,
            offset: 0,
            options,
            classes: Vec::new(),
            unresolved: Vec::new(),
            mismatches: Vec::new(),
            declared_properties: 0,
            read_properties: 0,
        }
    }

    fn read_schema(mut self) -> Result<Schema, SchemaError> {
        let mut top_level_class_count = 0;
        let consumed_bytes;
        loop {
            if self.at_terminator() {
                consumed_bytes = self.offset;
                self.offset += TERMINATOR_LEN;
                break;
            }
            if self.offset == self.data.len() {
                return Err(self.error("stream ended without its zero terminator", self.offset));
            }
            self.read_class(0, None, self.offset)?;
            top_level_class_count += 1;
        }

        let classes = self
            .classes
            .into_iter()
            .map(|class| class.expect("successful reads fill every registered class"))
            .collect();

        Ok(Schema {
            classes,
            top_level_class_count,
            property_count: self.declared_properties,
            parsed_property_count: self.read_properties,
            consumed_bytes,
            trailing_bytes: self.data[self.offset..].to_vec(),
            unresolved_references: self.unresolved,
            inline_index_mismatches: self.mismatches,
        })
    }

    fn read_class(
        &mut self,
        depth: usize,
        declared_index: Option<u16>,
        reference_offset: usize,
    ) -> Result<usize, SchemaError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(self.error("class nesting exceeds the depth bound", self.offset));
        }
        if self.classes.len() >= MAX_CLASSES {
            return Err(self.error("class count exceeds the safety bound", self.offset));
        }

        let slot = self.classes.len();
        let index = INITIAL_CLASS_INDEX + u16::try_from(slot).expect("class bound fits u16");
        let offset = self.offset;
        self.classes.push(None);

        let unknown_word = self.read_i16("class unknown word")?;
        let (name, name_bytes) = self.read_name_u16("class name")?;

        // Publish the name before reading references: a definition may refer to
        // itself while the rest of its record is still being read.
        self.classes[slot] = Some(ClassDefinition {
            index,
            name: name.clone(),
            name_bytes: name_bytes.clone(),
            parent: TypeReference::None,
            version: 0,
            properties: Vec::new(),
            guids: Vec::new(),
            unknown_word,
            inline: declared_index.is_some(),
            offset,
            end_offset: offset,
        });

        let parent = self.read_type_reference(depth)?;
        let version = self.read_i32("class version")?;
        let count_offset = self.offset;
        let property_count = self.read_i32("class property count")?;
        let property_count = self.checked_count(
            property_count,
            MIN_PROPERTY_LEN,
            "class property count",
            count_offset,
        )?;
        self.declared_properties = self
            .declared_properties
            .checked_add(property_count)
            .ok_or_else(|| self.error("declared property count overflows", count_offset))?;

        let mut properties = Vec::with_capacity(property_count);
        for _ in 0..property_count {
            properties.push(self.read_property(depth + 1)?);
        }

        let mut guids = Vec::new();
        if self.options.stream_version > 2 {
            let count_offset = self.offset;
            let guid_count = self.read_i32("class GUID count")?;
            let guid_count =
                self.checked_count(guid_count, GUID_LEN, "class GUID count", count_offset)?;
            guids.reserve(guid_count);
            for _ in 0..guid_count {
                let bytes = self.take(GUID_LEN, "class GUID")?;
                let mut guid = [0_u8; GUID_LEN];
                guid.copy_from_slice(bytes);
                guids.push(guid);
            }
        }

        let end_offset = self.offset;
        self.classes[slot] = Some(ClassDefinition {
            index,
            name,
            name_bytes,
            parent,
            version,
            properties,
            guids,
            unknown_word,
            inline: declared_index.is_some(),
            offset,
            end_offset,
        });

        if let Some(declared_index) = declared_index {
            if declared_index != index {
                self.mismatches.push(InlineIndexMismatch {
                    offset: reference_offset,
                    index,
                    declared_index,
                });
            }
        }
        Ok(slot)
    }

    fn read_type_reference(&mut self, depth: usize) -> Result<TypeReference, SchemaError> {
        let offset = self.offset;
        let word = self.read_u16("type reference")?;
        if word == 0 {
            return Ok(TypeReference::None);
        }
        if word & 0x8000 != 0 {
            let declared_index = word & 0x7fff;
            let slot = self.read_class(depth + 1, Some(declared_index), offset)?;
            let class = self.classes[slot]
                .as_ref()
                .expect("inline class is complete after read_class");
            return Ok(TypeReference::Inline {
                index: class.index,
                name: class.name.clone(),
                declared_index,
            });
        }

        let defined_class_count = self.classes.len();
        let slot = word
            .checked_sub(INITIAL_CLASS_INDEX)
            .map(usize::from)
            .filter(|slot| *slot < defined_class_count);
        let Some(slot) = slot else {
            self.unresolved.push(UnresolvedReference {
                offset,
                index: word,
                defined_class_count,
            });
            return Ok(TypeReference::Unresolved { index: word });
        };
        let name = self.classes[slot]
            .as_ref()
            .map_or_else(String::new, |class| class.name.clone());
        Ok(TypeReference::Reference { index: word, name })
    }

    fn read_property(&mut self, depth: usize) -> Result<PropertyDefinition, SchemaError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(self.error("property nesting exceeds the depth bound", self.offset));
        }
        self.read_properties += 1;
        if self.read_properties > MAX_PROPERTIES {
            return Err(self.error("property count exceeds the safety bound", self.offset));
        }

        let offset = self.offset;
        let (name, name_bytes) = self.read_name_i32("property name")?;
        let type_code = self.read_u8("property field type")?;
        let field_type = FieldType::from_code(type_code).ok_or_else(|| {
            self.error(
                format!("invalid property field type 0x{type_code:02x}"),
                offset,
            )
        })?;
        let raw_modes = self.read_u8("property modes")?;
        let loading_mode = raw_modes & 0x0f;
        let item_mode = i8::from_ne_bytes([raw_modes]) >> 4;
        let unknown_word = self.read_i16("property unknown word")?;
        let size = if item_mode == 1 {
            Some(self.read_i32("property item size")?)
        } else {
            None
        };

        let mut element = None;
        let mut static_type = None;
        let mut space_name_word = None;
        if loading_mode == 0 {
            if field_type == FieldType::Tuple {
                element = Some(Box::new(self.read_property(depth + 1)?));
            } else if field_type == FieldType::Object {
                static_type = Some(self.read_type_reference(depth + 1)?);
                if name == " " {
                    space_name_word = Some(self.read_i16("space-named property word")?);
                }
            }
        }

        Ok(PropertyDefinition {
            name,
            name_bytes,
            field_type,
            raw_modes,
            loading_mode,
            item_mode,
            unknown_word,
            size,
            element,
            static_type,
            space_name_word,
            offset,
        })
    }

    fn at_terminator(&self) -> bool {
        self.data
            .get(self.offset..self.offset.saturating_add(TERMINATOR_LEN))
            .is_some_and(|bytes| bytes.iter().all(|byte| *byte == 0))
    }

    fn checked_count(
        &self,
        value: i32,
        item_len: usize,
        what: &str,
        offset: usize,
    ) -> Result<usize, SchemaError> {
        let Ok(value) = usize::try_from(value) else {
            return Err(self.error(format!("{what} is negative"), offset));
        };
        if value > self.data.len().saturating_sub(self.offset) / item_len {
            return Err(self.error(
                format!("{what} {value} exceeds what the stream can hold"),
                offset,
            ));
        }
        Ok(value)
    }

    fn read_name_u16(&mut self, what: &str) -> Result<(String, Vec<u8>), SchemaError> {
        let length = usize::from(self.read_u16(&format!("{what} length"))?);
        self.read_name(length, what)
    }

    fn read_name_i32(&mut self, what: &str) -> Result<(String, Vec<u8>), SchemaError> {
        let offset = self.offset;
        let length = self.read_i32(&format!("{what} length"))?;
        let Ok(length) = usize::try_from(length) else {
            return Err(self.error(format!("{what} length is negative"), offset));
        };
        self.read_name(length, what)
    }

    fn read_name(&mut self, length: usize, what: &str) -> Result<(String, Vec<u8>), SchemaError> {
        let bytes = self.take(length, what)?.to_vec();
        let name = bytes.iter().copied().map(char::from).collect();
        Ok((name, bytes))
    }

    fn read_u8(&mut self, what: &str) -> Result<u8, SchemaError> {
        Ok(self.take(1, what)?[0])
    }

    fn read_u16(&mut self, what: &str) -> Result<u16, SchemaError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_i16(&mut self, what: &str) -> Result<i16, SchemaError> {
        let bytes = self.take(2, what)?;
        Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_i32(&mut self, what: &str) -> Result<i32, SchemaError> {
        let bytes = self.take(4, what)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn take(&mut self, length: usize, what: &str) -> Result<&'a [u8], SchemaError> {
        let start = self.offset;
        let Some(end) = start.checked_add(length) else {
            return Err(self.error(format!("{what} length overflows"), start));
        };
        let Some(bytes) = self.data.get(start..end) else {
            return Err(self.error(format!("truncated {what}"), start));
        };
        self.offset = end;
        Ok(bytes)
    }

    fn error(&self, message: impl Into<String>, offset: usize) -> SchemaError {
        SchemaError {
            message: message.into(),
            offset,
            classes_read: self.classes.len(),
            properties_read: self.read_properties,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_name_u16(bytes: &mut Vec<u8>, name: &[u8]) {
        bytes.extend(u16::try_from(name.len()).unwrap().to_le_bytes());
        bytes.extend(name);
    }

    fn push_name_i32(bytes: &mut Vec<u8>, name: &[u8]) {
        bytes.extend(i32::try_from(name.len()).unwrap().to_le_bytes());
        bytes.extend(name);
    }

    fn simple_property(name: &[u8], type_code: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_name_i32(&mut bytes, name);
        bytes.extend([type_code, 0]);
        bytes.extend(0_i16.to_le_bytes());
        bytes
    }

    fn class(name: &[u8], parent: &[u8], properties: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = 7_i16.to_le_bytes().to_vec();
        push_name_u16(&mut bytes, name);
        bytes.extend(parent);
        bytes.extend(3_i32.to_le_bytes());
        bytes.extend(i32::try_from(properties.len()).unwrap().to_le_bytes());
        for property in properties {
            bytes.extend(property);
        }
        bytes.extend(0_i32.to_le_bytes());
        bytes
    }

    #[test]
    fn parses_a_complete_class_and_preserves_trailing_bytes() {
        let mut data = class(
            b"Element",
            &0_u16.to_le_bytes(),
            &[simple_property(b"Id", 0x05)],
        );
        let consumed = data.len();
        data.extend([0; TERMINATOR_LEN]);
        data.extend([0xaa, 0xbb]);

        let schema = Schema::parse(&data).unwrap();
        assert_eq!(schema.consumed_bytes, consumed);
        assert_eq!(schema.trailing_bytes, [0xaa, 0xbb]);
        assert_eq!(schema.top_level_class_count, 1);
        assert_eq!(schema.property_count, 1);
        assert_eq!(schema.parsed_property_count, 1);
        assert_eq!(schema.classes[0].index, INITIAL_CLASS_INDEX);
        assert_eq!(schema.classes[0].name, "Element");
        assert_eq!(schema.classes[0].unknown_word, 7);
        assert_eq!(
            schema.classes[0].properties[0].field_type,
            FieldType::Integer32Alternate
        );
    }

    #[test]
    fn registers_inline_parent_after_its_child() {
        let parent_index = INITIAL_CLASS_INDEX + 1;
        let parent = class(b"Base", &0_u16.to_le_bytes(), &[]);
        let mut parent_ref = (0x8000 | parent_index).to_le_bytes().to_vec();
        parent_ref.extend(parent);
        let mut data = class(b"Child", &parent_ref, &[]);
        data.extend([0; TERMINATOR_LEN]);

        let schema = Schema::parse(&data).unwrap();
        assert_eq!(schema.classes.len(), 2);
        assert_eq!(schema.top_level_class_count, 1);
        assert_eq!(schema.classes[1].name, "Base");
        assert_eq!(schema.classes[1].index, parent_index);
        assert!(schema.classes[1].inline);
        assert_eq!(schema.classes[0].parent.name(), Some("Base"));
        assert!(schema.inline_index_mismatches.is_empty());
    }

    #[test]
    fn parses_tuple_and_object_descriptors() {
        let mut tuple = simple_property(b"Values", 0x0d);
        tuple.extend(simple_property(b" ", 0x0e));
        tuple.extend(INITIAL_CLASS_INDEX.to_le_bytes());
        tuple.extend(20_i16.to_le_bytes());
        let mut data = class(b"Element", &0_u16.to_le_bytes(), &[tuple]);
        data.extend([0; TERMINATOR_LEN]);

        let schema = Schema::parse(&data).unwrap();
        let tuple = &schema.classes[0].properties[0];
        let element = tuple.element.as_ref().unwrap();
        assert_eq!(element.field_type, FieldType::Object);
        assert_eq!(
            element.static_type.as_ref().unwrap().index(),
            Some(INITIAL_CLASS_INDEX)
        );
        assert_eq!(element.space_name_word, Some(20));
        assert_eq!(schema.property_count, 1);
        assert_eq!(schema.parsed_property_count, 2);
    }

    #[test]
    fn records_unresolved_references_without_repairing_them() {
        let mut data = class(b"Element", &400_u16.to_le_bytes(), &[]);
        data.extend([0; TERMINATOR_LEN]);
        let schema = Schema::parse(&data).unwrap();

        assert!(matches!(
            schema.classes[0].parent,
            TypeReference::Unresolved { index: 400 }
        ));
        assert_eq!(schema.unresolved_references[0].index, 400);
    }

    #[test]
    fn rejects_invalid_field_type_without_returning_partial_schema() {
        let mut data = class(
            b"Element",
            &0_u16.to_le_bytes(),
            &[simple_property(b"Bad", 0x0c)],
        );
        data.extend([0; TERMINATOR_LEN]);
        let error = Schema::parse(&data).unwrap_err();
        assert!(error.message().contains("invalid property field type"));
        assert_eq!(error.classes_read(), 1);
        assert_eq!(error.properties_read(), 1);
    }

    #[test]
    fn rejects_missing_terminator() {
        let data = class(b"Element", &0_u16.to_le_bytes(), &[]);
        let error = Schema::parse(&data).unwrap_err();
        assert!(error.message().contains("without its zero terminator"));
        assert_eq!(error.offset(), data.len());
    }
}

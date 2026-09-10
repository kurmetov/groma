//! Schema-driven walk of one serialized object body.
//!
//! The class schema declares, for every property, its field type, loading and
//! item modes, fixed sizes and nested element type. This module reads a record
//! body by following those declarations instead of hunting for offsets. The
//! walk asserts nothing on its own: a body is only accepted when the declared
//! properties tile it exactly, the same standard the member walk already uses.
//!
//! # Three readings that only work together
//!
//! Geometry records used to tile exactly 88.0% / 84.8% / 71.9% of the time on
//! the three corpus files, counting the face-bearing `GElement` records
//! geometry is actually read out of. They now tile **100%** - 13 886 / 7 712 /
//! 10 534 of 13 886 / 7 712 / 10 534 - and what closed the gap was dropping
//! three special cases rather than adding a fourth:
//!
//! 1. `GInfo.m_flags` was read at two bytes when the lead word's top bit was
//!    clear. It is read at its declared four bytes, like every other alternate
//!    integer.
//! 2. A reference inside an object rooted at `GNode` was read at a fixed six
//!    bytes, writing a class index even after a null identifier. It is read at
//!    the same variable width as every other reference: four bytes for a null.
//! 3. A `GFace` naming a filling was given two extra unexplained bytes, and
//!    `m_faceFlags_v9` - a property whose name carries a version gate - was
//!    skipped. The gated property is written, and it is those two bytes plus
//!    the two that reading nulls at six bytes was absorbing.
//!
//! All eight combinations were measured on BIG's face-bearing records. Each of
//! the three *alone* takes 71.9% to **0%**, as do two of the three pairs; the
//! third pair reaches 21.9%, still well below leaving everything alone. Only
//! the three together reach 100%. That is the shape of the thing: the old
//! readings were mutually compensating, each paying for another's two-byte
//! error, so every one of them measured worse in isolation than the wrong
//! reading it replaced - and no discriminator was ever going to rescue them
//! one at a time. The switch that measured the eight combinations was
//! temporary and is gone; `rivet flags-probe` is what remains, and it checks
//! the width the walk reads against the width the following bytes prove.
//!
//! What is *not* established is how to interpret `GInfo.m_flags`. Its four
//! bytes are accounted for, but nothing in the corpus separates "a four-byte
//! flags field" from "a two-byte flags field followed by two bytes belonging
//! to something undeclared", because no independent reading of the value
//! exists to check. The walk reads the declaration as declared, which is the
//! reading that needs no extra rule, and does not interpret the value.

use rvt_schema::{ClassDefinition, FieldType, PropertyDefinition, Schema, TypeReference};

use crate::{geometry::GElementNodeReference, member::MAX_STRING_CHARS};

/// A reference that names an object: identifier plus class index. A null
/// reference stops after the identifier and costs
/// [`IDENTIFIER_REFERENCE_BYTES`].
const OBJECT_REFERENCE_BYTES: usize = 6;
/// Short width of an `Integer32Alternate`. One field takes it: whichever
/// variable-width field opens a record body. See
/// [`FIRST_RECORD_IDENTIFIER_BYTES`].
const ALTERNATE_INTEGER32_BYTES: usize = 2;
/// Long width of an `Integer32Alternate`, which is its declared width and the
/// width every alternate integer but the record-opening one is written at.
///
/// Measured, not guessed. Reading them at the short width leaves the rest of
/// the record shifted two bytes early, which the record's own length hides
/// until the reference queue drains: the tail then reads as `0xffff_yyyy`
/// where an object's `GInfo.m_tag` should hold `0xffff_ffff`. The values the
/// declared width recovers corroborate it: `GFilling.m_fillColor` reads as
/// colours - 0x01000000 for the great majority, then 0x0000ffff, 0x00fdfdfd,
/// 0x0000bb00 - where the short read splits each colour across two fields.
///
/// `GInfo.m_flags` was read narrower than this for a while, on the reading
/// that the lead word's top bit marks a longer form. It does not: see
/// [`FlagWidthSample`] for the labelling that refuted it, and the module
/// header for what the three readings it was entangled with cost together.
const ALTERNATE_INTEGER32_LONG_BYTES: usize = 4;
/// Width read for `Integer16Alternate`, by the same reasoning.
const ALTERNATE_INTEGER16_BYTES: usize = 1;
/// Bit of the loading mode that marks a property holding references rather
/// than an inline object.
const REFERENCE_LOADING_BIT: u8 = 0x01;
/// Bit that narrows a reference to a bare identifier: the declaration already
/// fixes the class, so no class index is written. Measured on `GEdge`, whose
/// six face/next/previous links occupy twenty-four bytes, not thirty-six.
const IDENTIFIER_ONLY_LOADING_BIT: u8 = 0x02;
/// A bare identifier reference.
const IDENTIFIER_REFERENCE_BYTES: usize = 4;
/// Revit's invalid element identifier, which an unset `ElementId` field holds.
const INVALID_ELEMENT_ID: i32 = -1;
/// Width of the variable-width field that opens a record body, which is written
/// two bytes narrower than its declared four.
///
/// Measured, not guessed. On 17 `Element`-rooted record classes across roughly
/// 57 000 records of SMALL - `FamilyInstance`, `FamilySymbol`, `Level`,
/// `LeaderStyle`, `RbsPipeCurve`, `CategoryElem`, `GStyleElem` and ten more -
/// reading the record's first identifier at two bytes and every later one at
/// four lands `Element.m_id` on the record's own identifier in 100% of records;
/// reading the first at four lands it in 0%.
///
/// The same rule covers what an earlier reading called a two-byte prefix
/// between a record's declared properties and its node stream. A `GElement`
/// record's first variable-width field is `GInfo.m_flags`, not an identifier,
/// and reading that one narrow while every later alternate integer takes its
/// declared width explains those records byte for byte - the same 97.4% /
/// 98.3% / 97.6% as the prefix reading, with the same stop histogram - so one
/// rule replaces two. The prefix reading is what the same two bytes look like
/// when the narrowing is attributed to the end of the header instead of its
/// start.
///
/// Why the opening field is narrow is *not* established. Nothing in the corpus
/// separates "the record header omits the field's high half" from "the record
/// framing eats two bytes the body would otherwise carry", because the opening
/// field is the only place the narrow form appears.
const FIRST_RECORD_IDENTIFIER_BYTES: usize = 2;
/// Width of a reference's class index, written only when the identifier names
/// an object. A null reference - identifier zero - stops after the identifier.
const REFERENCE_CLASS_INDEX_BYTES: usize = 2;
/// Guard against a cyclic or malformed class chain.
const MAX_WALK_DEPTH: usize = 64;
/// Guard against a corrupt collection count claiming an absurd number of items.
const MAX_COLLECTION_ITEMS: u32 = 1 << 20;

/// Why a walk stopped before consuming the body exactly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SerialStop {
    /// The declared properties ran past the end of the body.
    Truncated { class: String, property: String },
    /// The walk met a declaration it does not know how to read.
    Unsupported {
        class: String,
        property: String,
        reason: &'static str,
    },
    /// The class chain could not be resolved against the schema.
    UnknownClass { class_index: u16 },
    /// The chain nested deeper than the walk accepts.
    TooDeep,
}

/// Result of walking one body against one class.
#[derive(Clone, Debug, PartialEq)]
pub struct SerialWalk {
    pub consumed: usize,
    /// Bytes left over after the declared properties were read.
    pub remaining: usize,
    /// Every node reference met, in the order they were read.
    pub references: Vec<GElementNodeReference>,
    pub stop: Option<SerialStop>,
}

impl SerialWalk {
    /// The body is explained exactly when nothing stopped the walk and no byte
    /// is left over.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        self.stop.is_none() && self.remaining == 0
    }
}

/// Walk `body` as an instance of `class_index`.
#[must_use]
pub fn walk_object(schema: &Schema, class_index: u16, body: &[u8]) -> SerialWalk {
    let mut reader = Reader {
        schema,
        body,
        offset: 0,
        references: Vec::new(),
        identifiers: Vec::new(),
        numbers: Vec::new(),
        integers: Vec::new(),
        strings: Vec::new(),
        small_integers: Vec::new(),
        alternate_integers: Vec::new(),
        node_headers: false,
        record_narrow_pending: true,
        trace: None,
        trace_properties: None,
        kept_strings: None,
        string_properties: None,
        // Neither walk reads a value back; both measure how much of a body the
        // declarations explain.
        collect_values: false,
        string_distance: 0,
        node_class: 0,
        flag_samples: None,
    };
    let stop = reader.read_class(class_index, 0).err();
    let consumed = reader.offset;
    SerialWalk {
        consumed,
        remaining: body.len().saturating_sub(consumed),
        references: reader.references,
        stop,
    }
}

/// Walk `body` as an instance of `class_index` and then keep walking the
/// objects its references name, in the order the references were read, until
/// the body is consumed. Newly met references join the back of the queue, so
/// a nested group contributes its own children.
#[must_use]
pub fn walk_object_stream(schema: &Schema, class_index: u16, body: &[u8]) -> SerialStreamWalk {
    let mut reader = Reader {
        schema,
        body,
        offset: 0,
        references: Vec::new(),
        identifiers: Vec::new(),
        numbers: Vec::new(),
        integers: Vec::new(),
        strings: Vec::new(),
        small_integers: Vec::new(),
        alternate_integers: Vec::new(),
        node_headers: false,
        record_narrow_pending: true,
        trace: None,
        trace_properties: None,
        kept_strings: None,
        string_properties: None,
        // Neither walk reads a value back; both measure how much of a body the
        // declarations explain.
        collect_values: false,
        string_distance: 0,
        node_class: 0,
        flag_samples: None,
    };
    let mut stop = reader.read_class(class_index, 0).err();
    let mut objects = 0_usize;
    let mut next = 0_usize;
    while stop.is_none() && reader.offset < body.len() {
        let Some(reference) = reader.references.get(next).copied() else {
            break;
        };
        next += 1;
        objects += 1;
        stop = reader.read_class(reference.class_index, 0).err();
    }
    let consumed = reader.offset;
    SerialStreamWalk {
        consumed,
        remaining: body.len().saturating_sub(consumed),
        objects,
        pending_references: reader.references.len().saturating_sub(next),
        references: reader.references,
        stop,
    }
}

/// Result of walking a record body as a header followed by its object stream.
#[derive(Clone, Debug, PartialEq)]
pub struct SerialStreamWalk {
    pub consumed: usize,
    pub remaining: usize,
    /// Referenced objects read after the header.
    pub objects: usize,
    /// References that were never reached because the body ran out first.
    pub pending_references: usize,
    pub references: Vec<GElementNodeReference>,
    pub stop: Option<SerialStop>,
}

/// Trailing `u32` that repeats the record's own body length.
pub const RECORD_LENGTH_TRAILER_BYTES: usize = 4;
/// Inline object that holds a live document handle rather than data. Its one
/// declared property, `m_pDoc`, is an identifier reference, which the
/// declaration would make four bytes wide; the width is in fact variable and a
/// null handle writes only a two-byte zero.
///
/// Measured, not guessed: reading the lead word and taking a zero as the whole
/// handle explains 92.8% / 91.9% / 90.1% of `GElement` records across the three
/// corpus files, against 91.4% / 82.0% / 83.7% for a fixed two bytes and
/// 75.1% / 83.9% / 81.1% for a fixed four. Every handle in the corpus holds one
/// of exactly two values - 0 written as two bytes (11 454 / 6 652 / 13 634
/// occurrences) and 1 written as four (5 205 / 25 994 / 56 501) - so "the lead
/// word is zero" and "the identifier is not 1" cannot be told apart here. The
/// lead-word form is the one a stream reader can apply, and is what is used.
const DOCUMENT_HANDLE_CLASS_NAME: &str = "ControlledConstDocAccess";
/// Width of a null document handle, whose lead word is zero.
const NULL_DOCUMENT_HANDLE_BYTES: usize = 2;
/// Width of a handle that names a document.
const DOCUMENT_HANDLE_BYTES: usize = 4;

/// Inline object every geometry-graph node inherits, whose `m_flags` closes
/// its header. The name is needed only by the width instrument, which samples
/// a node's own `GInfo` and nothing nested deeper.
const GINFO_CLASS_NAME: &str = "GInfo";
/// Class whose first declaration after the inherited `GNode.m_GInfo` names an
/// edge loop, which is what makes it checkable by the width oracle. See
/// [`flag_width_oracle`].
const FACE_CLASS_NAME: &str = "GFace";

/// An entity-map entry costs thirty-six bytes where its declarations account
/// for twenty-two. `ESEntityCell.m_entityMap` is a counted collection of
/// `std::pair< GUIDvalue, ESEntity >`, declared as a sixteen-byte key GUID and
/// one reference, `ESEntity.m_blob`. What is written is the key GUID, four
/// bytes of `0xffff_ffff`, and the key GUID again.
///
/// Measured, not guessed. All 5 478 entries in SMALL - one per `FamilyInstance`
/// record the walk could not explain, and the whole of that shortfall - hold a
/// collection count of one, the key GUID `2b2a021b 22578747 928a4f29 ca9d8811`,
/// the same four bytes, and a trailing GUID equal to the key in 5 478 of 5 478.
/// The bytes after the entry corroborate the width rather than merely fitting
/// it: at thirty-six the rest of the cell reads as its declarations say, with
/// `m_oFittingData`, `m_nodes` and `m_segments` naming classes 2444, 2447 and
/// 2448 - the classes those declarations name - and `m_baseElementId` holding a
/// live element identifier. At the declared twenty-two every one of those lands
/// mid-value.
///
/// The reading that the four bytes are a null identifier written the ordinary
/// way is *refuted*, not merely unused: `MEPAnalyticalModelCell.m_oFittingData`,
/// six bytes further into the same object, is also `0xffff_ffff` and *is*
/// followed by a class index. Whatever makes this reference four bytes wide is
/// not the identifier's value.
///
/// How the twenty unaccounted bytes divide is *not* established. "A four-byte
/// reference followed by a sixteen-byte GUID" and "a twenty-byte blob whose
/// tail happens to repeat the key" are byte-identical across the corpus,
/// because it holds exactly one extensible-storage schema and one entry per
/// map. The rule is applied at `m_blob`, the declaration the extra bytes
/// follow, and the sixteen are skipped rather than interpreted.
const ES_ENTITY_CLASS_NAME: &str = "ESEntity";
const ES_ENTITY_BLOB_PROPERTY: &str = "m_blob";
/// Bytes following `ESEntity.m_blob`'s identifier, holding the map key again.
const ES_ENTITY_TRAILING_BYTES: usize = 16;

/// Result of reading a whole record body: its own declared properties, then
/// the serialized nodes its references name, then the length trailer.
#[derive(Clone, Debug, PartialEq)]
pub struct SerialRecordWalk {
    pub consumed: usize,
    /// Offset the walk had reached when it stopped.
    pub stop_offset: usize,
    pub remaining: usize,
    /// Nodes read after the record's own properties.
    pub nodes: usize,
    /// Objects whose identifier no reference explained.
    pub pending_references: usize,
    /// Whether the trailing `u32` equals the body length.
    pub length_trailer_matches: bool,
    pub references: Vec<GElementNodeReference>,
    pub stop: Option<SerialStop>,
}

impl SerialRecordWalk {
    /// The record is explained when the walk consumed the body, the trailer
    /// agrees with the body length, and nothing stopped it. References left in
    /// the queue are not a fault: a record ends where its body ends, and the
    /// objects its last references name may live in another record.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        self.stop.is_none() && self.remaining == 0 && self.length_trailer_matches
    }
}

/// One `String` a record's declarations read, with the declaration that read
/// it. Where a scan has to decide which run of bytes looks like text, this says
/// which property the string is the value of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialString {
    pub offset: usize,
    pub class: String,
    pub property: String,
    pub value: String,
    /// How far the string sits from the record's own declarations: `0` for the
    /// record's own properties, `1` for an object one of them points at, and
    /// `2` for anything deeper in the node stream.
    pub distance: u8,
}

/// One property read, for diagnosing where a walk leaves the real layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialTraceEntry {
    pub offset: usize,
    pub class: String,
    pub property: String,
    pub consumed: usize,
}

/// One object read out of a record's node stream.
#[derive(Clone, Debug, PartialEq)]
pub struct SerialObject {
    /// Identifier of the reference that named this object.
    pub object_id: u32,
    pub class_index: u16,
    pub offset: usize,
    pub bytes: usize,
    /// Full references this object read, in order.
    pub references: Vec<GElementNodeReference>,
    /// Bare identifier references this object read, in order.
    pub identifiers: Vec<u32>,
    /// Every `Float64` this object read, in declaration order.
    pub numbers: Vec<f64>,
    /// Every `Integer32` this object read, in declaration order. An inline
    /// `ElementId` resolves to one of these, so a stored parameter's
    /// identifier lands here.
    pub integers: Vec<i32>,
    /// Every `String` this object read, in declaration order.
    pub strings: Vec<String>,
    /// Every `Bool`, `Integer8` or `Integer16` this object read, in
    /// declaration order and sign-extended to `i64`. Small flag/enum fields
    /// such as `GEdge.m_flags` live here rather than in `numbers`.
    pub small_integers: Vec<i64>,
    /// Every alternate integer this object read, in declaration order and
    /// zero-extended to `i64`: `GInfo.m_flags`, `GFace.m_faceFlags_v9` and
    /// the rest. These are bitfields, so the bytes are kept as written rather
    /// than sign-extended - a top bit set is bit 31, not a negative number.
    pub alternate_integers: Vec<i64>,
}

/// Same as [`walk`](walk_record), keeping each object the node stream held.
#[must_use]
pub fn walk_record_collecting(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
) -> (SerialRecordWalk, Vec<SerialObject>) {
    let options = RecordWalkOptions {
        collect: true,
        ..RecordWalkOptions::default()
    };
    let (walk, _, objects, _, _) = walk_record_inner(schema, class_index, body, options);
    (walk, objects)
}

/// Same as [`walk_record_collecting`], keeping only the objects whose class is
/// one of `keep`.
///
/// The walk is the same walk over the same bytes and stops where it always
/// stopped; what changes is which of the objects it meets are materialized.
/// An object is materialized by copying the seven value lists its
/// declarations filled, so a caller after the four parameter sets of a record
/// whose node stream holds sixty thousand faces was paying for all of them.
/// Values read outside a kept object are not retained either, since nothing
/// can read them back.
#[must_use]
pub fn walk_record_collecting_classes<'a>(
    schema: &'a Schema,
    class_index: u16,
    body: &'a [u8],
    keep: &'a [u16],
) -> (SerialRecordWalk, Vec<SerialObject>) {
    let options = RecordWalkOptions {
        collect: true,
        ..RecordWalkOptions::default()
    };
    let (walk, _, objects, _, _) =
        walk_record_inner_with(schema, class_index, body, options, None, Some(keep));
    (walk, objects)
}

/// Same as [`walk_record`], keeping every `String` the declarations read, in
/// the order they were read, with the declaration that read each one.
#[must_use]
pub fn walk_record_strings(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
) -> (SerialRecordWalk, Vec<SerialString>) {
    walk_record_strings_of(schema, class_index, body, None)
}

/// Same as [`walk_record_strings`], keeping only the strings read by one of
/// `properties`.
///
/// The walk is the same walk and reads the same bytes; what changes is how
/// much of it is retained. A caller after one named property - which is what
/// reading a record's name is - would otherwise pay three string allocations
/// for every value in the body to throw all but one of them away.
#[must_use]
pub fn walk_record_strings_of<'a>(
    schema: &'a Schema,
    class_index: u16,
    body: &'a [u8],
    properties: Option<&'a [&'a str]>,
) -> (SerialRecordWalk, Vec<SerialString>) {
    let options = RecordWalkOptions {
        keep_strings: true,
        ..RecordWalkOptions::default()
    };
    let (walk, _, _, strings, _) =
        walk_record_inner_with(schema, class_index, body, options, properties, None);
    (walk, strings)
}

/// Property whose value is the name of the element a record describes.
///
/// Measured, not guessed. Across SMALL's record classes this is where the names
/// actually live: `SymbolInfo.m_name` for every symbol class - `FamilySymbol`,
/// `LeaderStyle`, `TextNoteAttributes`, `SectionAttributes`,
/// `RbsWireInsulationType` and the rest - `FamilyBase.m_name` for a `Family`,
/// `FamilySurrogateBase.m_name` for a surrogate, `Category.m_name`,
/// `Font.m_name`, `LoadMiscBaseElem.m_name`. Where this and the offset scan it
/// replaces disagree, the declaration is right and the scan is reading a
/// neighbouring string: on `DimensionStyle` the scan returns the equality text
/// "EQ" for all 7 695 records while the declaration returns the style's own
/// name, and on `DBViewType` the scan returns the two-letter reference label
/// for all 12 272 while the declaration returns "План этажа" and its kind.
///
/// Taking the first string of any name instead would pull in
/// `ParamValueAString.m_value` - a stored parameter, not a name - for the
/// 11 974 `FamilyInstance` records, which is the failure this replaces.
const NAME_PROPERTY: &str = "m_name";
/// How far from the record's own declarations a name may sit. Zero is the
/// record's own properties, which is where `FamilyBase.m_name` and
/// `LoadMiscBaseElem.m_name` live; one is an object those properties point at,
/// which is where `SymbolInfo.m_name`, `Category.m_name` and `Font.m_name`
/// live, `Symbol.m_symbolInfo` being a reference rather than an inline object.
///
/// Anything further out belongs to something the element merely contains, not
/// to the element. Measured on the corpus: a `Family` whose own `m_name` is the
/// empty string has a `FamilySizeTableColumn.m_name` deeper in its node stream,
/// and taking that would name the family after a column of its size table.
const NAME_MAX_DISTANCE: u8 = 1;

/// Root of the class chain of a record that *is* a parameter's definition. Such
/// a record declares no [`NAME_PROPERTY`]; its name is
/// [`PARAMETER_CAPTION_CLASS`]`.`[`PARAMETER_CAPTION_PROPERTY`], reached through
/// `ParamElem.m_pParamDef`, which is a reference and so lands one step out.
const PARAMETER_ELEMENT_CLASS: &str = "ParamElem";
/// Class declaring the caption a parameter is displayed under.
const PARAMETER_CAPTION_CLASS: &str = "ParamDef";
/// Property whose value is a parameter's display name.
///
/// Measured, not guessed, and it corrects a real misreading. A parameter
/// element writes two strings and the *first* is not its name: `ParamElem`
/// declares `m_description` ahead of the `ParamDef` its `m_pParamDef` points
/// at, so the offset scan this replaces returned the description wherever one
/// was filled in - "Этаж, на котором располагается элемент" for the parameter
/// captioned "Этаж" (15 917 values on SMALL, the file's most-used project
/// parameter), "Вписывается назначение вида (План кладочный,
/// маркировочный...)", "Раздел проекта (АР, КЖ, ОВ и т.д.)". Those strings are
/// the tooltip Revit shows, not the name a specification is written under.
///
/// The class is pinned as well as the property because `m_caption` is declared
/// three times in the schema - also by `ColorFillData` and `ScheduleHeader` -
/// and only `ParamDef`'s is a parameter's name.
const PARAMETER_CAPTION_PROPERTY: &str = "m_caption";

/// The name a record's own declarations give it: the value of the first
/// property named [`NAME_PROPERTY`] the walk reads, skipping empty ones.
///
/// The record's own class chain is preferred over its node stream, because
/// both can declare the property and only the first is the record's own: a
/// `Family` carries `FamilyBase.m_name` in its header while its nodes hold a
/// `ParamDef.m_name` for every parameter the family defines. A record with no
/// such property in its header falls back to its nodes, which is where a
/// symbol keeps its name - `Symbol.m_symbolInfo` is a reference, so
/// `SymbolInfo.m_name` is read from the node stream.
///
/// A record whose declarations carry no such property at all has no name of
/// its own. A family instance is the common case: its name is its symbol's.
///
/// A parameter definition is the one class that names itself elsewhere; see
/// [`PARAMETER_CAPTION_PROPERTY`].
#[must_use]
pub fn record_name(schema: &Schema, class_index: u16, body: &[u8]) -> Option<String> {
    record_name_string(schema, class_index, body).map(|string| string.value)
}

/// Same as [`record_name`], keeping the declaration the name came from so a
/// caller measuring the reading can report which property answered.
#[must_use]
pub fn record_name_string(schema: &Schema, class_index: u16, body: &[u8]) -> Option<SerialString> {
    let parameter = descends_from(schema, class_index, PARAMETER_ELEMENT_CLASS);
    // The class chain decides which property can answer before the walk runs,
    // so the walk keeps that property and nothing else. The filter below is
    // unchanged and still checks the class as well: `m_caption` is declared by
    // three classes and only `ParamDef`'s is a name.
    let wanted: &[&str] = if parameter {
        &[PARAMETER_CAPTION_PROPERTY]
    } else {
        &[NAME_PROPERTY]
    };
    let (_walk, strings) = walk_record_strings_of(schema, class_index, body, Some(wanted));
    strings
        .into_iter()
        .filter(|string| {
            let declares_the_name = if parameter {
                string.class == PARAMETER_CAPTION_CLASS
                    && string.property == PARAMETER_CAPTION_PROPERTY
            } else {
                string.property == NAME_PROPERTY
            };
            declares_the_name && !string.value.is_empty() && string.distance <= NAME_MAX_DISTANCE
        })
        .min_by_key(|string| (string.distance, string.offset))
}

/// Whether `class_index` has `ancestor` anywhere in its class chain, itself
/// included.
#[must_use]
pub fn descends_from(schema: &Schema, class_index: u16, ancestor: &str) -> bool {
    let mut current = Some(class_index);
    for _ in 0..MAX_WALK_DEPTH {
        let Some(class) = current.and_then(|index| schema.class_by_index(index)) else {
            return false;
        };
        if class.name == ancestor {
            return true;
        }
        current = class.parent.index();
    }
    false
}

/// The value of one identifier-shaped property of a record's own header.
///
/// Only the record's own declarations are read - not the node stream - so the
/// answer is the record's own field and not a like-named one belonging to
/// something it merely contains. The property is located by name rather than by
/// offset, so the same call works for every class that declares it.
///
/// `None` when the class chain declares no such property, when the walk stops
/// before reaching it, or when the value is Revit's invalid identifier, `-1`.
#[must_use]
pub fn record_declared_id(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
    property: &str,
) -> Option<i32> {
    record_declared_ids(schema, class_index, body, &[property])
        .into_iter()
        .next()
        .flatten()
}

/// The values of several named identifier properties, read in one walk.
///
/// Same rule as [`record_declared_id`] for each, and one entry per requested
/// property in the order asked for. A class that declares a record's type, its
/// family and its category names them under three different properties, and
/// walking the declarations once for all of them costs what walking them once
/// for one does.
#[must_use]
pub fn record_declared_ids(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
    properties: &[&str],
) -> Vec<Option<i32>> {
    let mut reader = Reader {
        schema,
        body,
        offset: 0,
        references: Vec::new(),
        identifiers: Vec::new(),
        numbers: Vec::new(),
        integers: Vec::new(),
        strings: Vec::new(),
        small_integers: Vec::new(),
        alternate_integers: Vec::new(),
        node_headers: false,
        record_narrow_pending: true,
        trace: Some(Vec::new()),
        trace_properties: Some(properties),
        kept_strings: None,
        string_properties: None,
        collect_values: false,
        string_distance: 0,
        node_class: 0,
        flag_samples: None,
    };
    let _stop = reader.read_class(class_index, 0).err();
    let trace = reader.trace.unwrap_or_default();
    properties
        .iter()
        .map(|property| {
            let offset = trace
                .iter()
                .find(|entry| {
                    entry.property == *property && entry.consumed == IDENTIFIER_REFERENCE_BYTES
                })
                .map(|entry| entry.offset)?;
            let bytes = body.get(offset..offset.checked_add(IDENTIFIER_REFERENCE_BYTES)?)?;
            let value = i32::from_le_bytes(bytes.try_into().ok()?);
            (value != INVALID_ELEMENT_ID).then_some(value)
        })
        .collect()
}

/// Same as [`walk_record`], recording every property read.
#[must_use]
pub fn walk_record_traced(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
) -> (SerialRecordWalk, Vec<SerialTraceEntry>) {
    let options = RecordWalkOptions {
        trace: true,
        ..RecordWalkOptions::default()
    };
    let (walk, trace, _, _, _) = walk_record_inner(schema, class_index, body, options);
    (walk, trace)
}

/// Read `body` as a complete record: the declared properties of `class_index`,
/// a two-byte stream prefix, one serialized node per reference in the order
/// the references were read, and a trailing `u32` repeating the body length.
#[must_use]
pub fn walk_record(schema: &Schema, class_index: u16, body: &[u8]) -> SerialRecordWalk {
    walk_record_inner(schema, class_index, body, RecordWalkOptions::default()).0
}

/// One node `GInfo` met while walking a record, with everything its header
/// carries and - where the object's next declaration can be checked - the
/// width of `m_flags` that the bytes themselves prove.
///
/// This is an instrument, not a rule, and it labels a site without assuming
/// any rule: the width is read off the bytes that follow, so a reading can be
/// scored against the label rather than against itself. That is what refuted
/// taking the lead word's top bit as a width marker. It labels 108 136 /
/// 50 543 / 123 473 sites across the corpus at four bytes and not one site at
/// two, and every header field that reading would have needed a discriminator
/// in (`m_tag`, `m_controlCommand`, `m_categoryId`, and both words of
/// `m_flags` itself) is byte-identical between the loops it read correctly and
/// the 711 it did not. There was no discriminator to find.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlagWidthSample {
    /// Class of the node whose `GInfo` this is.
    pub node_class: u16,
    /// Offset of the `GInfo`'s first byte within the record body.
    pub offset: usize,
    pub tag: u32,
    pub control_command: u32,
    /// `GInfo.m_categoryId`, an inline `ElementId` holding one `Integer32`.
    pub category_id: i32,
    /// First word of `m_flags`. Its top bit was once read as a width marker;
    /// it is an ordinary flag bit, set on `Face` and `Edge` and clear on
    /// `EdgeLoop` and `GFilling`, which is the correlation that made the
    /// reading look right for as long as it did.
    pub lead: u16,
    /// Second word of `m_flags`, the half the narrow reading dropped.
    pub second: u16,
    /// The words from `m_flags` onwards, so a caller can read the object's
    /// next declaration under either width without a second walk. Words the
    /// body is too short for read zero.
    pub words: [u16; FLAG_SAMPLE_WORDS],
    /// Whether this object's next declaration is one the oracle can check.
    /// An unchecked object is still sampled, so a report can say how much of
    /// the corpus the check reaches rather than only what it found.
    pub checkable: bool,
    /// Whether reading `m_flags` at two bytes lands the object's next
    /// declaration on a live reference of the class the schema fixes.
    pub short_legal: bool,
    /// The same test with `m_flags` read at four bytes.
    pub long_legal: bool,
    /// Width the bytes prove, when exactly one of the two lands the object's
    /// next declaration on a legal reference. `None` when neither does or both
    /// do - an unlabelled sample, not a two-byte one.
    pub proved: Option<usize>,
    /// Width the walk read here, which is the declared four for every node
    /// `GInfo` in the corpus. Kept so a caller can report disagreement rather
    /// than assume there is none.
    pub read: usize,
}

/// Whether the reference following `GInfo` in a node of `node_class` can be
/// checked, and the class its target must descend from.
///
/// Two node classes qualify, and both for the same reason: their first
/// declaration after the inherited `GNode.m_GInfo` is a full reference - an
/// identifier and a class index - whose target class the schema fixes.
/// `GEdgeLoop.m_nextLoop` and `GFace.m_pFirstLoop` both name an edge loop, so
/// a correct read lands on one class index out of the schema's 4 418 and a
/// two-byte misread lands on whatever the following bytes happen to be.
/// `GEdge`'s six links are bare identifiers with no class index, so nothing
/// there separates a right read from a wrong one; edges are not sampled.
fn flag_width_oracle(schema: &Schema, node_class: u16) -> Option<&'static str> {
    if descends_from(schema, node_class, EDGE_LOOP_CLASS_NAME)
        || descends_from(schema, node_class, FACE_CLASS_NAME)
    {
        Some(EDGE_LOOP_CLASS_NAME)
    } else {
        None
    }
}

/// Class both checkable declarations name: `GEdgeLoop.m_nextLoop` and
/// `GFace.m_pFirstLoop`.
const EDGE_LOOP_CLASS_NAME: &str = "GEdgeLoop";
/// Depth at which a node's own `GInfo` properties are read: the node is read
/// at depth 1 and its inline `GInfo` one step in. A `GInfo` deeper than this
/// belongs to something the node contains, whose following declaration the
/// oracle does not know, so it is not sampled.
const NODE_GINFO_DEPTH: usize = 2;
/// Bytes of `GInfo` ahead of `m_flags`: `m_tag`, `m_controlCommand` and
/// `m_categoryId`, four each.
const GINFO_HEADER_BYTES: usize = 12;
/// Words kept from `m_flags` onwards: enough for the four-byte form and the
/// six-byte reference that may follow it, plus one either side.
pub const FLAG_SAMPLE_WORDS: usize = 6;

/// Walk a record exactly as [`walk_record`] does, sampling every node
/// `GInfo.m_flags` on the way so a caller can check the width the walk took
/// against the width the following bytes prove. See [`FlagWidthSample`].
#[must_use]
pub fn walk_record_flag_widths(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
) -> (SerialRecordWalk, Vec<FlagWidthSample>) {
    let options = RecordWalkOptions {
        flag_widths: true,
        ..RecordWalkOptions::default()
    };
    let (walk, _, _, _, samples) = walk_record_inner(schema, class_index, body, options);
    (walk, samples)
}

/// What one record walk should keep beyond the walk result itself.
#[derive(Clone, Copy, Debug, Default)]
// One independent switch per kind of output a caller can ask to keep.
#[allow(clippy::struct_excessive_bools)]
struct RecordWalkOptions {
    trace: bool,
    collect: bool,
    keep_strings: bool,
    /// Sample every node `GInfo.m_flags` against the width oracle. See
    /// [`FlagWidthSample`].
    flag_widths: bool,
}

fn walk_record_inner(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
    options: RecordWalkOptions,
) -> (
    SerialRecordWalk,
    Vec<SerialTraceEntry>,
    Vec<SerialObject>,
    Vec<SerialString>,
    Vec<FlagWidthSample>,
) {
    walk_record_inner_with(schema, class_index, body, options, None, None)
}

fn walk_record_inner_with<'a>(
    schema: &'a Schema,
    class_index: u16,
    body: &'a [u8],
    options: RecordWalkOptions,
    string_properties: Option<&'a [&'a str]>,
    collect_classes: Option<&'a [u16]>,
) -> (
    SerialRecordWalk,
    Vec<SerialTraceEntry>,
    Vec<SerialObject>,
    Vec<SerialString>,
    Vec<FlagWidthSample>,
) {
    let (trace, collect, keep_strings) = (options.trace, options.collect, options.keep_strings);
    let trailer_offset = body.len().saturating_sub(RECORD_LENGTH_TRAILER_BYTES);
    let length_trailer_matches = body
        .get(trailer_offset..)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .is_some_and(|bytes| u32::from_le_bytes(bytes) as usize == body.len());
    let mut reader = Reader {
        schema,
        body: &body[..trailer_offset],
        offset: 0,
        references: Vec::new(),
        identifiers: Vec::new(),
        numbers: Vec::new(),
        integers: Vec::new(),
        strings: Vec::new(),
        small_integers: Vec::new(),
        alternate_integers: Vec::new(),
        node_headers: false,
        record_narrow_pending: true,
        trace: trace.then(Vec::new),
        trace_properties: None,
        kept_strings: keep_strings.then(Vec::new),
        string_properties,
        // The record's own declarations are not part of any object, so their
        // values are kept only for a caller collecting every object.
        collect_values: collect && collect_classes.is_none(),
        string_distance: 0,
        node_class: 0,
        flag_samples: options.flag_widths.then(Vec::new),
    };
    let mut stop = reader.read_class(class_index, 0).err();
    // References read so far are the record's own: the objects they name sit
    // one step from the declarations, and everything they in turn name is
    // further out. See [`SerialString::distance`].
    let own_references = reader.references.len();
    reader.node_headers = true;
    let mut nodes = 0_usize;
    let mut next = 0_usize;
    let mut objects: Vec<SerialObject> = Vec::new();
    // Objects are written in the order their references were read: a node's
    // own references extend the queue behind those of its parent. A shared
    // object is written once per reference to it, not once overall: reading
    // each identifier only once leaves most records short.
    while stop.is_none() && reader.offset < reader.body.len() {
        let Some(reference) = reader.references.get(next).copied() else {
            break;
        };
        next += 1;
        // A null reference names no object and contributes no bytes.
        if reference.object_id == 0 || schema.class_by_index(reference.class_index).is_none() {
            continue;
        }
        nodes += 1;
        let kept =
            collect && collect_classes.is_none_or(|keep| keep.contains(&reference.class_index));
        reader.collect_values = kept;
        reader.node_class = reference.class_index;
        reader.string_distance = if next <= own_references { 1 } else { 2 };
        let began = reader.offset;
        let first_reference = reader.references.len();
        let first_identifier = reader.identifiers.len();
        let first_number = reader.numbers.len();
        let first_integer = reader.integers.len();
        let first_string = reader.strings.len();
        let first_small_integer = reader.small_integers.len();
        let first_alternate = reader.alternate_integers.len();
        stop = reader.read_class(reference.class_index, 1).err();
        if kept {
            objects.push(SerialObject {
                object_id: reference.object_id,
                class_index: reference.class_index,
                offset: began,
                bytes: reader.offset.saturating_sub(began),
                references: reader.references[first_reference..].to_vec(),
                identifiers: reader.identifiers[first_identifier..].to_vec(),
                numbers: reader.numbers[first_number..].to_vec(),
                integers: reader.integers[first_integer..].to_vec(),
                strings: reader.strings[first_string..].to_vec(),
                small_integers: reader.small_integers[first_small_integer..].to_vec(),
                alternate_integers: reader.alternate_integers[first_alternate..].to_vec(),
            });
        }
    }
    let consumed = reader.offset;
    (
        SerialRecordWalk {
            consumed,
            stop_offset: consumed,
            remaining: trailer_offset.saturating_sub(consumed),
            nodes,
            pending_references: reader.references.len().saturating_sub(next),
            length_trailer_matches,
            references: reader.references,
            stop,
        },
        reader.trace.unwrap_or_default(),
        objects,
        reader.kept_strings.unwrap_or_default(),
        reader.flag_samples.unwrap_or_default(),
    )
}

/// A record's UTF-16 string, as the format writes it: little-endian pairs,
/// zero-padded to the declared character count.
///
/// Built in one pass into one allocation. This is the same value
/// `String::from_utf16_lossy` followed by `trim_end_matches('\0')` produced -
/// an unpaired surrogate is still U+FFFD - without the two intermediate
/// buffers that spelling allocated on every string in every record.
fn decode_utf16_value(units: &[u8]) -> String {
    let mut value: String = char::decode_utf16(
        units
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
    )
    .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
    .collect();
    let kept = value.trim_end_matches('\0').len();
    value.truncate(kept);
    value
}

struct Reader<'a> {
    schema: &'a Schema,
    body: &'a [u8],
    offset: usize,
    references: Vec<GElementNodeReference>,
    /// Bare identifier references, which name an object without its class.
    identifiers: Vec<u32>,
    /// Every `Float64` read, so a caller can recover coordinates.
    numbers: Vec<f64>,
    /// Every `Integer32` read, in declaration order.
    integers: Vec<i32>,
    /// Every `String` read, in declaration order.
    strings: Vec<String>,
    /// Every `Bool`, `Integer8` or `Integer16` read, sign-extended to `i64`.
    small_integers: Vec<i64>,
    /// Every alternate integer read, zero-extended to `i64`.
    alternate_integers: Vec<i64>,
    /// Whether the walk is inside the node stream rather than the record's
    /// own declared properties.
    node_headers: bool,
    /// Whether the record body's first variable-width field is still to come.
    /// That field is written two bytes narrower than its declared width; every
    /// later one takes the declared width. See [`FIRST_RECORD_IDENTIFIER_BYTES`].
    record_narrow_pending: bool,
    trace: Option<Vec<SerialTraceEntry>>,
    /// Properties the trace is narrowed to, when a caller wants only some.
    ///
    /// The trace entry of a property carries its class and its own name, both
    /// owned, so tracing every property of every object allocated two strings
    /// per field read. A caller that walks a record to read four named
    /// identifiers out of it wants four entries, not the sixty thousand a
    /// `GElement` produces. `None` keeps every property, which is what the
    /// diagnostic walk still asks for.
    trace_properties: Option<&'a [&'a str]>,
    /// Every `String` read, with its declaration, when a caller asked for them.
    kept_strings: Option<Vec<SerialString>>,
    /// Properties the kept strings are narrowed to, on the same principle as
    /// [`Reader::trace_properties`]. Reading a record's name wants the one
    /// property that declares it, not the value of every string in the body.
    string_properties: Option<&'a [&'a str]>,
    /// Whether the per-type value lists are filled. A walk that reads only
    /// the shape of a record - the identifier reads, the framing checks -
    /// keeps none of them, and building a `String` per string field was the
    /// largest single cost of doing so.
    collect_values: bool,
    /// Distance of the object being read from the record's own declarations.
    /// See [`SerialString::distance`].
    string_distance: u8,
    /// Class of the node stream object being read, which decides whether the
    /// `GInfo` width oracle applies. See [`flag_width_oracle`].
    node_class: u16,
    /// Samples collected by the width instrument, when one is running.
    flag_samples: Option<Vec<FlagWidthSample>>,
}

impl Reader<'_> {
    /// Read every property of `class_index`, inherited properties first.
    fn read_class(&mut self, class_index: u16, depth: usize) -> Result<(), SerialStop> {
        if depth >= MAX_WALK_DEPTH {
            return Err(SerialStop::TooDeep);
        }
        // Into a fixed array rather than a fresh `Vec`: this runs once per
        // object read and a record's node stream holds hundreds of thousands
        // of them, so the allocation was paid more often than any other in
        // the walk. The depth is already bounded by `MAX_WALK_DEPTH`, and a
        // chain that would run past it is the same `TooDeep` it always was.
        let mut chain: [Option<&ClassDefinition>; MAX_WALK_DEPTH] = [None; MAX_WALK_DEPTH];
        let mut length = 0_usize;
        let mut current = Some(class_index);
        while let Some(index) = current {
            let Some(class) = self.schema.class_by_index(index) else {
                return Err(SerialStop::UnknownClass { class_index: index });
            };
            if length >= MAX_WALK_DEPTH {
                return Err(SerialStop::TooDeep);
            }
            chain[length] = Some(class);
            length += 1;
            current = class.parent.index();
        }
        for class in chain[..length].iter().rev().flatten() {
            for property in &class.properties {
                self.read_property(&class.name, property, depth)?;
            }
        }
        Ok(())
    }

    fn read_property(
        &mut self,
        class_name: &str,
        property: &PropertyDefinition,
        depth: usize,
    ) -> Result<(), SerialStop> {
        match property.item_mode {
            // A single value, and a string, which is one value however the
            // item mode is written.
            0 | 6 => self.read_item(class_name, property, depth),
            1 => {
                let count = property.size.unwrap_or(1).max(0);
                for _ in 0..count {
                    self.read_item(class_name, property, depth)?;
                }
                Ok(())
            }
            5 => {
                let count = self.take_u32().ok_or_else(|| SerialStop::Truncated {
                    class: class_name.to_owned(),
                    property: property.name.clone(),
                })?;
                if count > MAX_COLLECTION_ITEMS {
                    return Err(SerialStop::Unsupported {
                        class: class_name.to_owned(),
                        property: property.name.clone(),
                        reason: "collection count out of range",
                    });
                }
                for _ in 0..count {
                    self.read_item(class_name, property, depth)?;
                }
                Ok(())
            }
            _ => Err(SerialStop::Unsupported {
                class: class_name.to_owned(),
                property: property.name.clone(),
                reason: "item mode",
            }),
        }
    }

    fn read_item(
        &mut self,
        class_name: &str,
        property: &PropertyDefinition,
        depth: usize,
    ) -> Result<(), SerialStop> {
        let entered = self.offset;
        let traced = self.trace.is_some()
            && self
                .trace_properties
                .is_none_or(|wanted| wanted.contains(&property.name.as_str()));
        let result = self.read_item_inner(class_name, property, depth);
        if traced {
            let entry = SerialTraceEntry {
                offset: entered,
                class: class_name.to_owned(),
                property: property.name.clone(),
                consumed: self.offset.saturating_sub(entered),
            };
            if let Some(trace) = self.trace.as_mut() {
                trace.push(entry);
            }
        }
        result
    }

    #[allow(clippy::too_many_lines)] // One exhaustive match over every field type.
    fn read_item_inner(
        &mut self,
        class_name: &str,
        property: &PropertyDefinition,
        depth: usize,
    ) -> Result<(), SerialStop> {
        let truncated = || SerialStop::Truncated {
            class: class_name.to_owned(),
            property: property.name.clone(),
        };
        match property.field_type {
            FieldType::Tuple => {
                let Some(element) = property.element.as_deref() else {
                    return Err(SerialStop::Unsupported {
                        class: class_name.to_owned(),
                        property: property.name.clone(),
                        reason: "tuple without an element type",
                    });
                };
                self.read_property(class_name, element, depth + 1)
            }
            FieldType::Object => {
                let reference = property.loading_mode & REFERENCE_LOADING_BIT != 0;
                let identifier_only = property.loading_mode & IDENTIFIER_ONLY_LOADING_BIT != 0;
                if reference && identifier_only {
                    return self.take_identifier().ok_or_else(truncated);
                }
                // An entity-map entry runs past what its declarations account
                // for; the reference is four bytes wide and a repeat of the
                // map key follows it. See [`ES_ENTITY_CLASS_NAME`].
                if class_name == ES_ENTITY_CLASS_NAME && property.name == ES_ENTITY_BLOB_PROPERTY {
                    self.take_identifier().ok_or_else(&truncated)?;
                    return self.advance(ES_ENTITY_TRAILING_BYTES).ok_or_else(truncated);
                }
                if reference || identifier_only {
                    return self.take_reference().ok_or_else(truncated);
                }
                let Some(static_index) =
                    property.static_type.as_ref().and_then(TypeReference::index)
                else {
                    return Err(SerialStop::Unsupported {
                        class: class_name.to_owned(),
                        property: property.name.clone(),
                        reason: "inline object without a static type",
                    });
                };
                if self
                    .schema
                    .class_by_index(static_index)
                    .is_some_and(|class| class.name == DOCUMENT_HANDLE_CLASS_NAME)
                {
                    {
                        let width = if self.peek_u16().ok_or_else(&truncated)? == 0 {
                            NULL_DOCUMENT_HANDLE_BYTES
                        } else {
                            DOCUMENT_HANDLE_BYTES
                        };
                        return self.advance(width).ok_or_else(truncated);
                    }
                }
                self.read_class(static_index, depth + 1)
            }
            FieldType::String => {
                // A UTF-16 string: a character count, then two bytes each.
                let count = self.take_u32().ok_or_else(&truncated)?;
                if count > MAX_STRING_CHARS {
                    return Err(SerialStop::Unsupported {
                        class: class_name.to_owned(),
                        property: property.name.clone(),
                        reason: "string length out of range",
                    });
                }
                let bytes = usize::try_from(count)
                    .ok()
                    .and_then(|count| count.checked_mul(2))
                    .ok_or_else(&truncated)?;
                let units = self
                    .body
                    .get(self.offset..self.offset.saturating_add(bytes))
                    .ok_or_else(&truncated)?;
                let keep = self.kept_strings.is_some()
                    && self
                        .string_properties
                        .is_none_or(|wanted| wanted.contains(&property.name.as_str()));
                // Decoded once, into one allocation, and only where the value
                // is kept. Every walk used to build three strings per field -
                // the UTF-16 units, the lossy conversion, and the trimmed copy
                // - whether or not anything ever read them.
                if keep || self.collect_values {
                    let value = decode_utf16_value(units);
                    if keep {
                        let string = SerialString {
                            offset: self.offset,
                            class: class_name.to_owned(),
                            property: property.name.clone(),
                            value: value.clone(),
                            distance: self.string_distance,
                        };
                        if let Some(kept) = self.kept_strings.as_mut() {
                            kept.push(string);
                        }
                    }
                    if self.collect_values {
                        self.strings.push(value);
                    }
                }
                self.advance(bytes).ok_or_else(truncated)
            }
            FieldType::Integer32Alternate => {
                // Every alternate integer takes its declared width, except
                // the one that opens a record body.
                let width = if self.take_narrow() {
                    ALTERNATE_INTEGER32_BYTES
                } else {
                    ALTERNATE_INTEGER32_LONG_BYTES
                };
                if self.flag_samples.is_some()
                    && self.node_headers
                    && class_name == GINFO_CLASS_NAME
                    && depth == NODE_GINFO_DEPTH
                {
                    if let Some(sample) = self.sample_flag_width(width) {
                        if let Some(samples) = self.flag_samples.as_mut() {
                            samples.push(sample);
                        }
                    }
                }
                self.push_alternate(width);
                self.advance(width).ok_or_else(truncated)
            }
            FieldType::Integer16Alternate => {
                self.push_alternate(ALTERNATE_INTEGER16_BYTES);
                self.advance(ALTERNATE_INTEGER16_BYTES)
                    .ok_or_else(truncated)
            }
            FieldType::Float64 => {
                let bytes = self
                    .body
                    .get(self.offset..self.offset.saturating_add(8))
                    .ok_or_else(&truncated)?;
                let value = f64::from_le_bytes(bytes.try_into().map_err(|_| truncated())?);
                self.numbers.push(value);
                self.offset += 8;
                Ok(())
            }
            FieldType::Bool | FieldType::Integer8 => {
                let byte = *self.body.get(self.offset).ok_or_else(&truncated)?;
                let value = if property.field_type == FieldType::Bool {
                    i64::from(byte)
                } else {
                    i64::from(i8::from_ne_bytes([byte]))
                };
                self.small_integers.push(value);
                self.offset += 1;
                Ok(())
            }
            FieldType::Integer32 => {
                let bytes = self
                    .body
                    .get(self.offset..self.offset.saturating_add(4))
                    .ok_or_else(&truncated)?;
                let value = i32::from_le_bytes(bytes.try_into().map_err(|_| truncated())?);
                self.integers.push(value);
                self.offset += 4;
                Ok(())
            }
            FieldType::Integer16 => {
                let bytes = self
                    .body
                    .get(self.offset..self.offset.saturating_add(2))
                    .ok_or_else(&truncated)?;
                let value = i16::from_le_bytes(bytes.try_into().map_err(|_| truncated())?);
                self.small_integers.push(i64::from(value));
                self.offset += 2;
                Ok(())
            }
            other => {
                let width = other.fixed_width().ok_or_else(|| SerialStop::Unsupported {
                    class: class_name.to_owned(),
                    property: property.name.clone(),
                    reason: "field type without a width",
                })?;
                self.advance(width).ok_or_else(truncated)
            }
        }
    }

    /// Keep the value of an alternate integer about to be stepped over.
    ///
    /// The bytes are taken as written and zero-extended: these fields are
    /// bitfields - `GInfo.m_flags`, `GFace.m_faceFlags_v9` - so bit 31 set is
    /// a bit, not a sign. A field the body is too short for keeps nothing,
    /// which is the same thing the walk does with it.
    fn push_alternate(&mut self, width: usize) {
        let Some(bytes) = self
            .offset
            .checked_add(width)
            .and_then(|end| self.body.get(self.offset..end))
        else {
            return;
        };
        let mut value = 0_u32;
        for (index, byte) in bytes.iter().take(4).enumerate() {
            value |= u32::from(*byte) << (8 * index);
        }
        self.alternate_integers.push(i64::from(value));
    }

    fn advance(&mut self, bytes: usize) -> Option<()> {
        let end = self.offset.checked_add(bytes)?;
        (end <= self.body.len()).then(|| {
            self.offset = end;
        })
    }

    fn peek_u16(&self) -> Option<u16> {
        let bytes = self.body.get(self.offset..self.offset.checked_add(2)?)?;
        Some(u16::from_le_bytes(bytes.try_into().ok()?))
    }

    fn take_u32(&mut self) -> Option<u32> {
        let bytes = self.body.get(self.offset..self.offset.checked_add(4)?)?;
        let value = u32::from_le_bytes(bytes.try_into().ok()?);
        self.offset += 4;
        Some(value)
    }

    fn take_identifier(&mut self) -> Option<()> {
        let end = self.offset.checked_add(IDENTIFIER_REFERENCE_BYTES)?;
        let bytes = self.body.get(self.offset..end)?;
        self.identifiers
            .push(u32::from_le_bytes(bytes.try_into().ok()?));
        self.offset = end;
        Some(())
    }

    /// Whether this is the record body's first variable-width field, which is
    /// written narrow. Claims the narrow form, so only one field gets it.
    fn take_narrow(&mut self) -> bool {
        let narrow = self.record_narrow_pending;
        self.record_narrow_pending = false;
        narrow
    }

    /// Read a reference: an identifier of four bytes - two if it opens the
    /// record body - followed by a class index only when the identifier names
    /// an object. One encoding, everywhere; see the module header for the
    /// geometry-graph exception this used to carry.
    fn take_reference(&mut self) -> Option<()> {
        let identifier_bytes = if self.take_narrow() {
            FIRST_RECORD_IDENTIFIER_BYTES
        } else {
            IDENTIFIER_REFERENCE_BYTES
        };
        let end = self.offset.checked_add(identifier_bytes)?;
        let bytes = self.body.get(self.offset..end)?;
        let mut identifier = [0_u8; 4];
        identifier[..identifier_bytes].copy_from_slice(bytes);
        let object_id = u32::from_le_bytes(identifier);
        self.offset = end;
        if object_id == 0 {
            self.references.push(GElementNodeReference {
                object_id: 0,
                class_index: 0,
            });
            return Some(());
        }
        let end = self.offset.checked_add(REFERENCE_CLASS_INDEX_BYTES)?;
        let bytes = self.body.get(self.offset..end)?;
        self.references.push(GElementNodeReference {
            object_id,
            class_index: u16::from_le_bytes(bytes.try_into().ok()?),
        });
        self.offset = end;
        Some(())
    }

    fn u32_at(&self, at: usize) -> Option<u32> {
        let bytes = self.body.get(at..at.checked_add(4)?)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn i32_at(&self, at: usize) -> Option<i32> {
        let bytes = self.body.get(at..at.checked_add(4)?)?;
        Some(i32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn u16_at(&self, at: usize) -> Option<u16> {
        let bytes = self.body.get(at..at.checked_add(2)?)?;
        Some(u16::from_le_bytes(bytes.try_into().ok()?))
    }

    /// Whether the six bytes at `at` read as a live reference to a class
    /// descending from `ancestor`. This is the oracle's whole test: a null
    /// identifier is not counted, because a null is legal wherever it lands
    /// and so separates nothing.
    fn names_a(&self, at: usize, ancestor: &str) -> bool {
        if self.body.len() < at.saturating_add(OBJECT_REFERENCE_BYTES) {
            return false;
        }
        let Some(object_id) = self.u32_at(at) else {
            return false;
        };
        if object_id == 0 {
            return false;
        }
        self.u16_at(at.saturating_add(IDENTIFIER_REFERENCE_BYTES))
            .is_some_and(|class_index| descends_from(self.schema, class_index, ancestor))
    }

    /// Sample this node's `GInfo` header and, where the object's next
    /// declaration is checkable, the width the bytes prove. See
    /// [`FlagWidthSample`].
    fn sample_flag_width(&self, read: usize) -> Option<FlagWidthSample> {
        let ancestor = flag_width_oracle(self.schema, self.node_class);
        let header = self.offset.checked_sub(GINFO_HEADER_BYTES)?;
        let short_legal = ancestor.is_some_and(|ancestor| {
            self.names_a(self.offset + ALTERNATE_INTEGER32_BYTES, ancestor)
        });
        let long_legal = ancestor.is_some_and(|ancestor| {
            self.names_a(self.offset + ALTERNATE_INTEGER32_LONG_BYTES, ancestor)
        });
        Some(FlagWidthSample {
            checkable: ancestor.is_some(),
            node_class: self.node_class,
            offset: header,
            tag: self.u32_at(header)?,
            control_command: self.u32_at(header + 4)?,
            category_id: self.i32_at(header + 8)?,
            lead: self.peek_u16()?,
            second: self.u16_at(self.offset + 2)?,
            words: std::array::from_fn(|word| {
                self.u16_at(self.offset + word * 2).unwrap_or_default()
            }),
            short_legal,
            long_legal,
            proved: match (short_legal, long_legal) {
                (true, false) => Some(ALTERNATE_INTEGER32_BYTES),
                (false, true) => Some(ALTERNATE_INTEGER32_LONG_BYTES),
                _ => None,
            },
            read,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rvt_schema::{ClassDefinition, TypeReference};

    /// `class_by_index` addresses `classes` from this index.
    const FIRST_CLASS_INDEX: u16 = rvt_schema::INITIAL_CLASS_INDEX;
    /// The variable-width field that opens a record body, written narrow.
    const NARROW_FLAGS: u16 = 0x0004;
    /// An alternate integer anywhere else, at its declared four bytes. The
    /// value is one the corpus holds for a node `GInfo.m_flags`, whose lead
    /// word has the top bit clear: that bit is a flag, not a width marker.
    const FLAGS: u32 = 0x0008_0004;

    fn property(
        name: &str,
        field_type: FieldType,
        loading_mode: u8,
        item_mode: i8,
        size: Option<i32>,
    ) -> PropertyDefinition {
        PropertyDefinition {
            name: name.to_owned(),
            name_bytes: name.as_bytes().to_vec(),
            field_type,
            raw_modes: loading_mode,
            loading_mode,
            item_mode,
            unknown_word: 0,
            size,
            element: None,
            static_type: None,
            space_name_word: None,
            offset: 0,
        }
    }

    fn class(
        index: u16,
        name: &str,
        parent: TypeReference,
        properties: Vec<PropertyDefinition>,
    ) -> ClassDefinition {
        ClassDefinition {
            index,
            name: name.to_owned(),
            name_bytes: name.as_bytes().to_vec(),
            parent,
            version: 1,
            properties,
            guids: Vec::new(),
            unknown_word: 0,
            inline: false,
            offset: 0,
            end_offset: 0,
        }
    }

    /// A base class holding an inline object, and a derived class holding a
    /// reference collection and a fixed tuple, mirroring `GNode`/`GGroup`.
    fn schema() -> Schema {
        let mut identifier = property("m_id", FieldType::Object, 0x00, 0, None);
        identifier.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX,
            name: "Identifier".to_owned(),
        });
        let mut coordinates = property("m_coord", FieldType::Tuple, 0x10, 1, Some(2));
        coordinates.element = Some(Box::new(property(
            "element",
            FieldType::Float64,
            0x10,
            1,
            Some(3),
        )));
        Schema {
            classes: vec![
                class(
                    FIRST_CLASS_INDEX,
                    "Identifier",
                    TypeReference::None,
                    vec![property("m_id", FieldType::Integer32, 0x00, 0, None)],
                ),
                class(
                    FIRST_CLASS_INDEX + 1,
                    "Base",
                    TypeReference::None,
                    vec![
                        property("m_tag", FieldType::Integer32, 0x00, 0, None),
                        identifier,
                        property("m_flags", FieldType::Integer32Alternate, 0x00, 0, None),
                    ],
                ),
                class(
                    FIRST_CLASS_INDEX + 2,
                    "Derived",
                    TypeReference::Reference {
                        index: FIRST_CLASS_INDEX + 1,
                        name: "Base".to_owned(),
                    },
                    vec![
                        property("m_subNodes", FieldType::Object, 0x51, 5, None),
                        coordinates,
                    ],
                ),
            ],
            top_level_class_count: 3,
            property_count: 6,
            parsed_property_count: 6,
            consumed_bytes: 0,
            trailing_bytes: Vec::new(),
            unresolved_references: Vec::new(),
            inline_index_mismatches: Vec::new(),
        }
    }

    fn derived_body(node_count: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(7_u32.to_le_bytes()); // Base.m_tag
        body.extend((-1_i32).to_le_bytes()); // Base.m_id, one inline Identifier
        body.extend(0x0008_u16.to_le_bytes()); // Base.m_flags, two bytes
        body.extend(node_count.to_le_bytes());
        for index in 0..node_count {
            body.extend((index + 3).to_le_bytes());
            body.extend(2_081_u16.to_le_bytes());
        }
        for value in [-1.0_f64, -2.0, -3.0, 1.0, 2.0, 3.0] {
            body.extend(value.to_le_bytes());
        }
        body
    }

    #[test]
    fn inherited_declarations_tile_a_body_exactly() {
        let schema = schema();
        let walk = walk_object(&schema, FIRST_CLASS_INDEX + 2, &derived_body(2));
        assert_eq!(walk.stop, None);
        assert!(walk.is_exact());
        // 10 header bytes, a 4-byte count, two 6-byte references, six f64.
        assert_eq!(walk.consumed, 10 + 4 + 12 + 48);
        assert_eq!(
            walk.references,
            [
                GElementNodeReference {
                    object_id: 3,
                    class_index: 2_081,
                },
                GElementNodeReference {
                    object_id: 4,
                    class_index: 2_081,
                },
            ]
        );
    }

    /// A record shaped like the corpus: the root's declared properties, whose
    /// first variable-width field is the inline `GInfo` flags word and so is
    /// written narrow, one node whose own `GInfo` ends in the variable-width
    /// flags word, and the length trailer.
    fn record_schema() -> Schema {
        let mut info = property("m_GInfo", FieldType::Object, 0x00, 0, None);
        info.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX,
            name: GINFO_CLASS_NAME.to_owned(),
        });
        let mut node_info = info.clone();
        node_info.name = "m_GInfo".to_owned();
        Schema {
            classes: vec![
                class(
                    FIRST_CLASS_INDEX,
                    GINFO_CLASS_NAME,
                    TypeReference::None,
                    vec![
                        property("m_tag", FieldType::Integer32, 0x00, 0, None),
                        property("m_flags", FieldType::Integer32Alternate, 0x00, 0, None),
                    ],
                ),
                class(
                    FIRST_CLASS_INDEX + 1,
                    "Root",
                    TypeReference::None,
                    vec![
                        info,
                        property("m_subNodes", FieldType::Object, 0x51, 5, None),
                    ],
                ),
                class(
                    FIRST_CLASS_INDEX + 2,
                    "Node",
                    TypeReference::None,
                    vec![
                        node_info,
                        property("m_endParams", FieldType::Float64, 0x10, 1, Some(2)),
                    ],
                ),
            ],
            top_level_class_count: 3,
            property_count: 5,
            parsed_property_count: 5,
            consumed_bytes: 0,
            trailing_bytes: Vec::new(),
            unresolved_references: Vec::new(),
            inline_index_mismatches: Vec::new(),
        }
    }

    #[test]
    fn a_record_is_its_properties_then_its_nodes_then_its_length() {
        let node_class = FIRST_CLASS_INDEX + 2;
        let mut body = Vec::new();
        body.extend(6_i32.to_le_bytes()); // Root's inline GInfo
        body.extend(NARROW_FLAGS.to_le_bytes()); // narrow: it opens the body
        body.extend(1_u32.to_le_bytes()); // one sub-node
        body.extend(3_u32.to_le_bytes());
        body.extend(node_class.to_le_bytes());
        body.extend((-1_i32).to_le_bytes()); // the node's inline GInfo
        body.extend(FLAGS.to_le_bytes());
        body.extend(0.0_f64.to_le_bytes());
        body.extend(287.5_f64.to_le_bytes());
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let schema = record_schema();
        let walk = walk_record(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert_eq!(walk.stop, None);
        assert!(walk.length_trailer_matches);
        assert_eq!(walk.nodes, 1);
        assert_eq!(walk.pending_references, 0);
        assert!(walk.is_exact());

        // The trailer must repeat the body length, not merely be present.
        let mut wrong_length = body.clone();
        let last = wrong_length.len() - RECORD_LENGTH_TRAILER_BYTES;
        wrong_length[last] = wrong_length[last].wrapping_add(1);
        assert!(!walk_record(&schema, FIRST_CLASS_INDEX + 1, &wrong_length).is_exact());

        // The same body with the node's flags word written at two bytes is
        // not explained. The walk reads that field at its declared four
        // wherever it appears but the record's opening field, and a node's
        // `GInfo` is never the opening field.
        let mut short_form = body.clone();
        let flags_at = body.len() - RECORD_LENGTH_TRAILER_BYTES - 16 - 4;
        short_form.drain(flags_at..flags_at + 2);
        let length = u32::try_from(short_form.len()).unwrap();
        let last = short_form.len() - RECORD_LENGTH_TRAILER_BYTES;
        short_form[last..].copy_from_slice(&length.to_le_bytes());
        assert!(!walk_record(&schema, FIRST_CLASS_INDEX + 1, &short_form).is_exact());
    }

    #[test]
    fn every_alternate_integer_but_the_record_opening_one_takes_its_declared_width() {
        // A node shaped like `GFilling`: its inline `GInfo`, then a colour
        // declared `Integer32Alternate`. Both take the declared four bytes;
        // only the field that opens the record body is narrow.
        let mut classes = record_schema().classes;
        classes[2].properties = vec![
            classes[2].properties[0].clone(),
            property("m_fillColor", FieldType::Integer32Alternate, 0x00, 0, None),
        ];
        let schema = Schema {
            classes,
            ..record_schema()
        };
        let node_class = FIRST_CLASS_INDEX + 2;

        let mut body = Vec::new();
        body.extend(6_i32.to_le_bytes()); // Root's inline GInfo
        body.extend(NARROW_FLAGS.to_le_bytes()); // narrow: it opens the body
        body.extend(1_u32.to_le_bytes()); // one sub-node
        body.extend(3_u32.to_le_bytes());
        body.extend(node_class.to_le_bytes());
        body.extend((-1_i32).to_le_bytes()); // the node's inline GInfo
        body.extend(FLAGS.to_le_bytes());
        body.extend(0x0100_0000_u32.to_le_bytes()); // m_fillColor
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let walk = walk_record(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert_eq!(walk.stop, None);
        assert!(walk.is_exact());
        assert_eq!(walk.nodes, 1);
    }

    #[test]
    fn small_integer_fields_are_captured_in_declaration_order() {
        // A node shaped like `GEdge`: its inline `GInfo`, then an `Integer8`
        // flags byte (mirroring `GEdge.m_flags`) and a `Bool`.
        let mut classes = record_schema().classes;
        classes[2]
            .properties
            .push(property("m_flags", FieldType::Integer8, 0x00, 0, None));
        classes[2]
            .properties
            .push(property("m_open", FieldType::Bool, 0x00, 0, None));
        let schema = Schema {
            classes,
            ..record_schema()
        };
        let node_class = FIRST_CLASS_INDEX + 2;

        let mut body = Vec::new();
        body.extend(6_i32.to_le_bytes()); // Root's inline GInfo
        body.extend(NARROW_FLAGS.to_le_bytes()); // narrow: it opens the body
        body.extend(1_u32.to_le_bytes()); // one sub-node
        body.extend(3_u32.to_le_bytes());
        body.extend(node_class.to_le_bytes());
        body.extend((-1_i32).to_le_bytes()); // the node's inline GInfo
        body.extend(FLAGS.to_le_bytes());
        body.extend(0.0_f64.to_le_bytes());
        body.extend(287.5_f64.to_le_bytes());
        body.push((-2_i8).to_le_bytes()[0]); // m_flags: a negative Integer8
        body.push(1_u8); // m_open: true
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let (walk, objects) = walk_record_collecting(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert!(walk.is_exact());
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].small_integers, [-2, 1]);
    }

    #[test]
    fn reference_width_follows_the_loading_mode_and_nulls_hold_no_body() {
        let mut classes = record_schema().classes;
        // A node holding one full reference, two identifier-only links, and a
        // property whose name carries a version gate. The gated property is
        // written like any other: skipping it was one of the three readings
        // that only balanced each other. See the module header.
        classes[2].properties = vec![
            classes[2].properties[0].clone(),
            property("m_pSurf", FieldType::Object, 0x01, 0, None),
            property("m_pFace", FieldType::Object, 0x03, 1, Some(2)),
            property(
                "m_faceFlags_v9",
                FieldType::Integer32Alternate,
                0x00,
                0,
                None,
            ),
        ];
        let schema = Schema {
            classes,
            ..record_schema()
        };
        let node_class = FIRST_CLASS_INDEX + 2;

        let mut body = Vec::new();
        body.extend(6_i32.to_le_bytes());
        body.extend(NARROW_FLAGS.to_le_bytes());
        body.extend(2_u32.to_le_bytes()); // two sub-nodes: one real, one null
        body.extend(3_u32.to_le_bytes());
        body.extend(node_class.to_le_bytes());
        body.extend(0_u32.to_le_bytes()); // the null: identifier only
        body.extend((-1_i32).to_le_bytes());
        body.extend(FLAGS.to_le_bytes());
        body.extend(9_u32.to_le_bytes()); // m_pSurf: identifier and class
        body.extend(565_u16.to_le_bytes());
        body.extend(11_u32.to_le_bytes()); // m_pFace: two bare identifiers
        body.extend(12_u32.to_le_bytes());
        body.extend(FLAGS.to_le_bytes()); // m_faceFlags_v9, written
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let walk = walk_record(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert_eq!(walk.stop, None);
        assert_eq!(walk.remaining, 0);

        // Omitting the gated field leaves the record four bytes short.
        let mut without = body.clone();
        let gated_at = body.len() - RECORD_LENGTH_TRAILER_BYTES - 4;
        without.drain(gated_at..gated_at + 4);
        let length = u32::try_from(without.len()).unwrap();
        let last = without.len() - RECORD_LENGTH_TRAILER_BYTES;
        without[last..].copy_from_slice(&length.to_le_bytes());
        assert!(!walk_record(&schema, FIRST_CLASS_INDEX + 1, &without).is_exact());
        assert!(walk.length_trailer_matches);
        // One node was read, and the body ended there. The null reference and
        // the surface this fixture points at are both left outside the body,
        // so they stay pending rather than being read.
        assert_eq!(walk.nodes, 1);
        assert_eq!(walk.pending_references, 2);
        assert_eq!(
            walk.references.last(),
            Some(&GElementNodeReference {
                object_id: 9,
                class_index: 565,
            })
        );
    }

    #[test]
    fn the_field_that_opens_a_record_body_is_written_narrow() {
        // A record whose first declared property is a reference: its
        // identifier holds two bytes, not the four a later one would.
        let mut classes = record_schema().classes;
        classes[1].properties = vec![
            property("m_pHead", FieldType::Object, 0x01, 0, None),
            property("m_pTail", FieldType::Object, 0x01, 0, None),
        ];
        let schema = Schema {
            classes,
            ..record_schema()
        };
        let node_class = FIRST_CLASS_INDEX + 2;

        let mut body = Vec::new();
        body.extend(0xffff_u16.to_le_bytes()); // m_pHead: a narrow identifier
        body.extend(node_class.to_le_bytes());
        body.extend(0_u32.to_le_bytes()); // m_pTail: null, identifier only
        body.extend((-1_i32).to_le_bytes()); // the node's inline GInfo
        body.extend(FLAGS.to_le_bytes());
        body.extend(0.0_f64.to_le_bytes());
        body.extend(287.5_f64.to_le_bytes());
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let walk = walk_record(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert_eq!(walk.stop, None);
        assert!(walk.is_exact());
        assert_eq!(walk.nodes, 1);
        assert_eq!(
            walk.references,
            [
                GElementNodeReference {
                    object_id: 0xffff,
                    class_index: node_class,
                },
                GElementNodeReference {
                    object_id: 0,
                    class_index: 0,
                },
            ]
        );

        // The same body with two more bytes in the opening identifier is not
        // explained: the narrow form is the record's, not this fixture's.
        let mut wide = body.clone();
        wide.splice(2..2, [0_u8; 2]);
        let length = u32::try_from(wide.len()).unwrap();
        let last = wide.len() - RECORD_LENGTH_TRAILER_BYTES;
        wide[last..].copy_from_slice(&length.to_le_bytes());
        assert!(!walk_record(&schema, FIRST_CLASS_INDEX + 1, &wide).is_exact());
    }

    #[test]
    fn the_width_instrument_labels_an_edge_loop_from_the_reference_that_follows() {
        // An `EdgeLoop`-shaped node: `GNode.m_GInfo`, then `m_nextLoop`, a
        // full reference the schema fixes the class of, then `m_pFace`.
        //
        // The bytes are the corpus's own. A loop that names a next loop reads
        // `0004 0008 <live reference>`, and a terminal one reads
        // `0004 0008 0000 0000 <m_pFace>`, where two-byte flags would make
        // `m_nextLoop` the nonsense `id=8 class=0`. Both are read at the
        // declared four bytes, and the instrument proves four for the first
        // and declines to label the second, which is what the corpus shows:
        // 108 136 / 50 543 / 123 473 sites proving four and none proving two.
        let mut classes = record_schema().classes;
        classes.push(class(
            FIRST_CLASS_INDEX + 3,
            "GNode",
            TypeReference::None,
            Vec::new(),
        ));
        let loop_class = FIRST_CLASS_INDEX + 4;
        let mut edge_loop = classes[2].clone();
        edge_loop.index = loop_class;
        edge_loop.name = EDGE_LOOP_CLASS_NAME.to_owned();
        edge_loop.parent = TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 3,
            name: "GNode".to_owned(),
        };
        edge_loop.properties = vec![
            classes[2].properties[0].clone(),
            property("m_nextLoop", FieldType::Object, 0x01, 0, None),
            property("m_pFace", FieldType::Object, 0x03, 0, None),
        ];
        classes.push(edge_loop);
        let schema = Schema {
            classes,
            ..record_schema()
        };

        let record = |next_loop: u32| {
            let mut body = Vec::new();
            body.extend(6_i32.to_le_bytes()); // Root's inline GInfo
            body.extend(NARROW_FLAGS.to_le_bytes());
            body.extend(1_u32.to_le_bytes()); // one sub-node
            body.extend(3_u32.to_le_bytes());
            body.extend(loop_class.to_le_bytes());
            body.extend((-1_i32).to_le_bytes()); // the loop's inline GInfo
            body.extend(FLAGS.to_le_bytes());
            body.extend(next_loop.to_le_bytes());
            if next_loop != 0 {
                body.extend(loop_class.to_le_bytes());
            }
            body.extend(4_u32.to_le_bytes()); // m_pFace, a bare identifier
            let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
            body.extend(length.to_le_bytes());
            body
        };

        let root = FIRST_CLASS_INDEX + 1;
        let (walk, samples) = walk_record_flag_widths(&schema, root, &record(50));
        assert!(walk.is_exact());
        assert_eq!(samples.len(), 1);
        assert!(samples[0].checkable);
        assert_eq!(samples[0].proved, Some(ALTERNATE_INTEGER32_LONG_BYTES));
        assert_eq!(samples[0].read, ALTERNATE_INTEGER32_LONG_BYTES);
        assert_eq!(samples[0].lead, NARROW_FLAGS);
        assert_eq!(samples[0].second, 0x0008);

        // A terminal loop cannot be labelled: a null identifier is legal
        // wherever it lands, so it separates nothing. The instrument reports
        // that rather than counting it as a two-byte site.
        let (walk, samples) = walk_record_flag_widths(&schema, root, &record(0));
        assert!(walk.is_exact());
        assert_eq!(samples.len(), 1);
        assert!(samples[0].checkable);
        assert_eq!(samples[0].proved, None);
        assert!(!samples[0].short_legal);
        assert!(!samples[0].long_legal);
    }

    #[test]
    fn a_null_reference_stops_after_its_identifier_in_a_geometry_node_too() {
        // Two nodes of the same shape, one rooted at the geometry graph and
        // one not. The null reference costs four bytes in both: the class
        // index is written only when the identifier names an object.
        //
        // The geometry root used to be an exception here, and that reading is
        // what made a null `GEdgeLoop.m_nextLoop` cost the same six bytes as
        // a two-byte `m_flags` plus a four-byte null - the coincidence that
        // hid the flags width for as long as it did. See the module header.
        let mut classes = record_schema().classes;
        classes[2].properties = vec![
            classes[2].properties[0].clone(),
            property("m_pSurf", FieldType::Object, 0x01, 0, None),
        ];
        classes.push(class(
            FIRST_CLASS_INDEX + 3,
            "GNode",
            TypeReference::None,
            Vec::new(),
        ));
        let mut geometry_node = classes[2].clone();
        geometry_node.index = FIRST_CLASS_INDEX + 4;
        geometry_node.name = FACE_CLASS_NAME.to_owned();
        geometry_node.parent = TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 3,
            name: "GNode".to_owned(),
        };
        classes.push(geometry_node);
        let schema = Schema {
            classes,
            ..record_schema()
        };

        let record = |node_class: u16, null_bytes: usize| {
            let mut body = Vec::new();
            body.extend(6_i32.to_le_bytes()); // Root's inline GInfo
            body.extend(NARROW_FLAGS.to_le_bytes());
            body.extend(1_u32.to_le_bytes()); // one sub-node
            body.extend(3_u32.to_le_bytes());
            body.extend(node_class.to_le_bytes());
            body.extend((-1_i32).to_le_bytes()); // the node's inline GInfo
            body.extend(FLAGS.to_le_bytes());
            body.extend(vec![0_u8; null_bytes]); // m_pSurf: null
            let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
            body.extend(length.to_le_bytes());
            body
        };

        let root = FIRST_CLASS_INDEX + 1;
        for node in [FIRST_CLASS_INDEX + 2, FIRST_CLASS_INDEX + 4] {
            assert!(
                walk_record(&schema, root, &record(node, IDENTIFIER_REFERENCE_BYTES)).is_exact()
            );
            assert!(!walk_record(&schema, root, &record(node, OBJECT_REFERENCE_BYTES)).is_exact());
        }
    }

    #[test]
    fn an_entity_map_entry_carries_a_repeat_of_its_key() {
        // `ESEntityCell` shaped as the corpus declares it: a counted collection
        // of `std::pair< GUIDvalue, ESEntity >`, where the entry occupies
        // thirty-six bytes against the twenty-two the declarations account for.
        // The integer after the collection is the alignment check: it only
        // reads as its value when the entry is taken at its measured width.
        let mut key = property("first", FieldType::Object, 0x00, 0, None);
        key.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 3,
            name: "GUIDvalue".to_owned(),
        });
        let mut value = property("second", FieldType::Object, 0x00, 0, None);
        value.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 4,
            name: ES_ENTITY_CLASS_NAME.to_owned(),
        });
        let mut entity_map = property("m_entityMap", FieldType::Object, 0x50, 5, None);
        entity_map.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 5,
            name: "std::pair< GUIDvalue, ESEntity >".to_owned(),
        });

        let mut classes = record_schema().classes;
        classes[1].properties = vec![
            entity_map,
            property("m_id", FieldType::Integer32, 0x00, 0, None),
        ];
        classes.push(class(
            FIRST_CLASS_INDEX + 3,
            "GUIDvalue",
            TypeReference::None,
            vec![property("m_guid", FieldType::Guid, 0x00, 0, None)],
        ));
        classes.push(class(
            FIRST_CLASS_INDEX + 4,
            ES_ENTITY_CLASS_NAME,
            TypeReference::None,
            vec![property(
                ES_ENTITY_BLOB_PROPERTY,
                FieldType::Object,
                0x01,
                0,
                None,
            )],
        ));
        classes.push(class(
            FIRST_CLASS_INDEX + 5,
            "std::pair< GUIDvalue, ESEntity >",
            TypeReference::None,
            vec![key, value],
        ));
        let schema = Schema {
            classes,
            ..record_schema()
        };

        let guid = [0x2b_u8; 16];
        let record = |trailing: &[u8]| {
            let mut body = Vec::new();
            body.extend(1_u32.to_le_bytes()); // one entry in the map
            body.extend(guid); // the key
            body.extend((-1_i32).to_le_bytes()); // ESEntity.m_blob
            body.extend(trailing);
            body.extend(0x0007_10cf_u32.to_le_bytes()); // the alignment check
            let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
            body.extend(length.to_le_bytes());
            body
        };

        let root = FIRST_CLASS_INDEX + 1;
        let walk = walk_record(&schema, root, &record(&guid));
        assert_eq!(walk.stop, None);
        assert!(walk.is_exact());
        // Without the repeat of the key the entry is the declared twenty-two
        // bytes wide, and the walk runs off the end of the body instead.
        assert!(!walk_record(&schema, root, &record(&[])).is_exact());
    }

    #[test]
    fn a_declared_identifier_is_found_by_name_in_the_record_s_own_header() {
        // A record declaring an inline `ElementId` after the narrow opening
        // field, and a node whose class declares a property of the same name.
        let mut type_id = property("m_masterSymbolId", FieldType::Object, 0x00, 0, None);
        type_id.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 3,
            name: "ElementId".to_owned(),
        });
        let mut classes = record_schema().classes;
        classes[1].properties = vec![
            property("m_flags", FieldType::Integer32Alternate, 0x00, 0, None),
            type_id.clone(),
            property("m_subNodes", FieldType::Object, 0x51, 5, None),
        ];
        classes[2].properties = vec![type_id];
        classes.push(class(
            FIRST_CLASS_INDEX + 3,
            "ElementId",
            TypeReference::None,
            vec![property("m_id", FieldType::Integer32, 0x00, 0, None)],
        ));
        let schema = Schema {
            classes,
            ..record_schema()
        };

        let record = |own: i32, node: i32| {
            let mut body = Vec::new();
            body.extend(NARROW_FLAGS.to_le_bytes()); // the narrow opening field
            body.extend(own.to_le_bytes());
            body.extend(1_u32.to_le_bytes()); // one sub-node
            body.extend(3_u32.to_le_bytes());
            body.extend((FIRST_CLASS_INDEX + 2).to_le_bytes());
            body.extend(node.to_le_bytes());
            let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
            body.extend(length.to_le_bytes());
            body
        };

        let root = FIRST_CLASS_INDEX + 1;
        let read = |body: &[u8]| record_declared_id(&schema, root, body, "m_masterSymbolId");
        // The record's own value, not the like-named one its node carries.
        assert_eq!(read(&record(217_275, 999)), Some(217_275));
        // Revit's invalid identifier is not an element.
        assert_eq!(read(&record(-1, 999)), None);
        // A property the class chain does not declare has no value here.
        assert_eq!(
            record_declared_id(&schema, root, &record(217_275, 999), "m_hostId"),
            None
        );

        // No one property names an element's type across every class - a
        // loadable family calls it `m_masterSymbolId`, a system family
        // `m_idType` - so callers try candidates in order. That is only sound
        // because a class declaring one of them yields nothing for the others,
        // whatever the bytes at that offset happen to say.
        let mut system = Schema {
            classes: schema.classes.clone(),
            ..record_schema()
        };
        system.classes[1].properties[1].name = "m_idType".to_owned();
        system.classes[1].properties[1].name_bytes = b"m_idType".to_vec();
        assert_eq!(
            record_declared_id(&system, root, &record(310_493, 999), "m_masterSymbolId"),
            None
        );
        assert_eq!(
            record_declared_id(&system, root, &record(310_493, 999), "m_idType"),
            Some(310_493)
        );
    }

    #[test]
    fn a_name_is_the_record_s_own_before_the_objects_it_points_at() {
        // Three classes each declaring `m_name`: the record's own, the object
        // its declarations point at, and one a step further out.
        let mut classes = record_schema().classes;
        classes[1].properties = vec![
            property("m_name", FieldType::String, 0x60, 6, None),
            property("m_pChild", FieldType::Object, 0x01, 0, None),
        ];
        classes[2].properties = vec![
            property("m_name", FieldType::String, 0x60, 6, None),
            property("m_pGrandchild", FieldType::Object, 0x01, 0, None),
        ];
        classes.push(class(
            FIRST_CLASS_INDEX + 3,
            "Grandchild",
            TypeReference::None,
            vec![property("m_name", FieldType::String, 0x60, 6, None)],
        ));
        let schema = Schema {
            classes,
            ..record_schema()
        };
        let child_class = FIRST_CLASS_INDEX + 2;
        let grandchild_class = FIRST_CLASS_INDEX + 3;

        let utf16 = |text: &str| {
            let mut bytes = Vec::new();
            let units = text.encode_utf16().collect::<Vec<_>>();
            bytes.extend(u32::try_from(units.len()).unwrap().to_le_bytes());
            for unit in units {
                bytes.extend(unit.to_le_bytes());
            }
            bytes
        };

        let record = |own: &str| {
            let mut body = Vec::new();
            body.extend(utf16(own)); // the record's own name
            body.extend(0xffff_u16.to_le_bytes()); // m_pChild: a narrow identifier
            body.extend(child_class.to_le_bytes());
            body.extend(utf16("child")); // the child, one step out
            body.extend(7_u32.to_le_bytes()); // its m_pGrandchild
            body.extend(grandchild_class.to_le_bytes());
            body.extend(utf16("grandchild")); // two steps out
            let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
            body.extend(length.to_le_bytes());
            body
        };

        let root = FIRST_CLASS_INDEX + 1;
        assert!(walk_record(&schema, root, &record("")).is_exact());
        // An empty own name is no name, so the object it points at answers -
        // but never the one beyond that.
        assert_eq!(
            record_name(&schema, root, &record("")),
            Some("child".to_owned())
        );
        assert_eq!(
            record_name(&schema, root, &record("mine")),
            Some("mine".to_owned())
        );
        let strings = walk_record_strings(&schema, root, &record("mine")).1;
        assert_eq!(
            strings
                .iter()
                .map(|string| (string.value.as_str(), string.distance))
                .collect::<Vec<_>>(),
            [("mine", 0), ("child", 1), ("grandchild", 2)]
        );
    }

    #[test]
    fn a_parameter_element_is_named_by_its_definition_s_caption_not_its_description() {
        // The real layout: `ParamElem` writes its description first and then a
        // reference to the `ParamDef` that carries the caption. Anything
        // reading the record's first string returns the description.
        let mut classes = record_schema().classes;
        classes[1].name = PARAMETER_ELEMENT_CLASS.to_owned();
        classes[1].properties = vec![
            property("m_description", FieldType::String, 0x60, 6, None),
            property("m_pParamDef", FieldType::Object, 0x01, 0, None),
        ];
        classes[2].name = PARAMETER_CAPTION_CLASS.to_owned();
        classes[2].properties = vec![
            property("m_dynamicGroupName", FieldType::String, 0x60, 6, None),
            property(PARAMETER_CAPTION_PROPERTY, FieldType::String, 0x60, 6, None),
            // A parameter element declares no `m_name` anywhere, so the rule
            // that names every other class cannot answer here.
            property("m_name", FieldType::String, 0x60, 6, None),
        ];
        let schema = Schema {
            classes,
            ..record_schema()
        };
        let definition_class = FIRST_CLASS_INDEX + 2;

        let utf16 = |text: &str| {
            let mut bytes = Vec::new();
            let units = text.encode_utf16().collect::<Vec<_>>();
            bytes.extend(u32::try_from(units.len()).unwrap().to_le_bytes());
            for unit in units {
                bytes.extend(unit.to_le_bytes());
            }
            bytes
        };
        let record = |description: &str| {
            let mut body = Vec::new();
            body.extend(utf16(description));
            body.extend(0xffff_u16.to_le_bytes()); // m_pParamDef, narrow: opens the body
            body.extend(definition_class.to_le_bytes());
            body.extend(utf16("Текст")); // m_dynamicGroupName
            body.extend(utf16("Этаж")); // m_caption
            body.extend(utf16("не имя")); // m_name, one step out and not the name
            let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
            body.extend(length.to_le_bytes());
            body
        };

        let root = FIRST_CLASS_INDEX + 1;
        let description = "Этаж, на котором располагается элемент";
        assert!(walk_record(&schema, root, &record(description)).is_exact());
        assert_eq!(
            record_name(&schema, root, &record(description)),
            Some("Этаж".to_owned())
        );
        // The caption answers whether or not a description was filled in, so
        // the name does not change shape between two parameters of one file.
        assert_eq!(
            record_name(&schema, root, &record("")),
            Some("Этаж".to_owned())
        );
        assert_eq!(
            record_name_string(&schema, root, &record(description)).map(|string| string.class),
            Some(PARAMETER_CAPTION_CLASS.to_owned())
        );
        // Only a parameter element is named this way: the same body read as a
        // class that does not descend from `ParamElem` falls back to `m_name`.
        let mut other = Schema {
            classes: schema.classes.clone(),
            ..record_schema()
        };
        other.classes[1].name = "Root".to_owned();
        assert_eq!(
            record_name(&other, root, &record(description)),
            Some("не имя".to_owned())
        );
    }

    #[test]
    fn a_document_handle_is_two_bytes_when_null_and_four_when_it_names_a_document() {
        let mut classes = record_schema().classes;
        let mut handle = property("m_cda", FieldType::Object, 0x00, 0, None);
        handle.static_type = Some(TypeReference::Reference {
            index: FIRST_CLASS_INDEX + 3,
            name: DOCUMENT_HANDLE_CLASS_NAME.to_owned(),
        });
        classes[2].properties = vec![classes[2].properties[0].clone(), handle];
        classes.push(class(
            FIRST_CLASS_INDEX + 3,
            DOCUMENT_HANDLE_CLASS_NAME,
            TypeReference::None,
            // Declared as an identifier reference, yet a null handle writes
            // only two bytes: the handle points at a live document, not at
            // data.
            vec![property("m_pDoc", FieldType::Object, 0x03, 0, None)],
        ));
        let schema = Schema {
            classes,
            ..record_schema()
        };

        let mut body = Vec::new();
        body.extend(6_i32.to_le_bytes());
        body.extend(NARROW_FLAGS.to_le_bytes());
        body.extend(1_u32.to_le_bytes());
        body.extend(3_u32.to_le_bytes());
        body.extend((FIRST_CLASS_INDEX + 2).to_le_bytes());
        body.extend((-1_i32).to_le_bytes());
        body.extend(FLAGS.to_le_bytes());
        let prefix = body.clone();
        body.extend([0_u8; NULL_DOCUMENT_HANDLE_BYTES]);
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let walk = walk_record(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert_eq!(walk.stop, None);
        assert!(walk.is_exact());
        assert_eq!(walk.nodes, 1);

        // The same node with a handle that names document 1 writes four bytes.
        let mut body = prefix;
        body.extend(1_u32.to_le_bytes());
        let length = u32::try_from(body.len() + RECORD_LENGTH_TRAILER_BYTES).unwrap();
        body.extend(length.to_le_bytes());

        let walk = walk_record(&schema, FIRST_CLASS_INDEX + 1, &body);
        assert_eq!(walk.stop, None);
        assert!(walk.is_exact());
        assert_eq!(walk.nodes, 1);
    }

    #[test]
    fn reports_a_body_that_the_declarations_do_not_explain() {
        let schema = schema();
        let mut short = derived_body(1);
        short.truncate(short.len() - 1);
        let walk = walk_object(&schema, FIRST_CLASS_INDEX + 2, &short);
        assert!(!walk.is_exact());
        assert!(matches!(walk.stop, Some(SerialStop::Truncated { .. })));

        let mut padded = derived_body(1);
        padded.extend([0_u8; 3]);
        let walk = walk_object(&schema, FIRST_CLASS_INDEX + 2, &padded);
        assert_eq!(walk.stop, None);
        assert_eq!(walk.remaining, 3);
        assert!(!walk.is_exact());
    }
}

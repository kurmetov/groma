#![forbid(unsafe_code)]

mod brep;
mod compound;
mod elem_table;
mod geometry;
mod member;
mod parameter;
mod serial;

pub use brep::{
    BrepArc, BrepClassIndexes, BrepCurve, BrepEdge, BrepExclusion, BrepFace, BrepLoop,
    BrepOpenness, BrepProfile, BrepRuling, BrepSurface, HoleTally, OrderingControl, SymbolBrep,
    assemble as assemble_symbol_brep,
};
pub use compound::{
    COMPOUND_STRUCTURE_CLASS_NAME, COMPOUND_STRUCTURE_LAYER_CLASS_NAME, CompoundLayer,
    CompoundStructure, CompoundStructureClassIndexes, HOST_OBJECT_ATTRIBUTES_CLASS_NAME,
};
pub use elem_table::{
    ElemTable, ElemTableError, ElemTableHeader, ElemTableLayout, ElemTableRecord, RecordFraming,
};
pub use geometry::{
    FamilyInstancePlacementFields, FittingCenterLineFields, GElementBounds, GElementGraphFields,
    GElementNodeReference, GInstanceTransformFields, PipeLineGeometryFields, RvtPoint3,
};
pub use member::{
    ELEMENT_HEADER_ID_BLOCK_BYTES, ELEMENT_ID_SEARCH_WINDOW_BYTES, ELEMENT_PRE_ID_WORD_BYTES,
    ELEMENT_TAIL_BYTES, ElementAnchor, ElementFields, ElementHeaderFields,
    LEVEL_SERIALIZED_PLANE_BYTES, LevelFields, MAX_STRING_CHARS, MIN_STRING_CHARS, MemberRecord,
    MemberRecordError, MemberRecords, MemberWalk, RecordHeader, RecordLayout, RecordString,
};
pub use parameter::{
    AUTODESK_SPEC_PREFIX, MAX_PARAMETERS_PER_SET, Parameter, ParameterSetClassIndexes,
    ParameterSets, ParameterSpec, ParameterValue,
};
pub use serial::{
    FLAG_SAMPLE_WORDS, FlagWidthSample, RECORD_LENGTH_TRAILER_BYTES, SerialObject,
    SerialRecordWalk, SerialStop, SerialStreamWalk, SerialString, SerialTraceEntry, SerialWalk,
    descends_from, record_declared_id, record_declared_ids, record_name, record_name_string,
    walk_object, walk_object_stream, walk_record, walk_record_collecting, walk_record_flag_widths,
    walk_record_strings, walk_record_traced,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RvtObject {
    pub id: Option<i64>,
    pub class_name: Option<String>,
    pub fields: Vec<RvtField>,
    pub raw_source: RawReference,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RvtField {
    pub name: Option<String>,
    pub value: RvtValue,
    /// Original bytes for lossless investigation when available.
    pub raw: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RvtValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Bytes(Vec<u8>),
    ObjectRef(i64),
    Array(Vec<Self>),
    Struct(Vec<RvtField>),
    Unknown(Vec<u8>),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RawReference {
    pub stream: String,
    pub offset: u64,
    pub length: u64,
}

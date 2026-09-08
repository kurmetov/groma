//! The layer table a compound host object's type carries.
//!
//! A wall, floor, ceiling or roof is not one material: its type owns a
//! `CompoundStructure`, and that owns an ordered list of
//! `CompoundStructureLayer`. Both are declared classes, so this reads the
//! declarations rather than searching a body:
//!
//! ```text
//! HostObjAttr.m_pCompoundStructure  -> CompoundStructure   (a reference)
//! CompoundStructure.m_layers        -> N x CompoundStructureLayer (inline)
//! CompoundStructureLayer.m_layerWidth   Float64, Revit internal feet
//! CompoundStructureLayer.m_materialId   ElementId of a `MaterialElem`
//! ```
//!
//! `m_pCompoundStructure` is a reference, so the structure is one object of
//! the record's node stream and arrives as its own [`SerialObject`].
//! `m_layers` is a counted collection with a static element class and loading
//! mode 0, so its layers are walked *inline* into that same object: each
//! layer's declared fields land in the structure object's collected values, in
//! declaration order. That is the same pairing-by-declaration the parameter
//! sets are read with, and it is what fixes the layer order - the file writes
//! the layers in the order the type lists them.
//!
//! Per layer the declarations contribute one `Float64` (`m_layerWidth`), five
//! `Integer32` (`m_layerFunction`, `m_embeddingType`, the `Identifier.m_id`
//! inside each of `m_materialId` and `m_profileId`, and `m_layerId`) and one
//! `Bool` (`m_layerCapFlag`). `CompoundStructure` itself declares no
//! `Float64` at all, so the count of collected doubles *is* the layer count,
//! and the integers that follow the layers are the structure's own scalars.
//! [`CompoundStructureClassIndexes::detect`] checks that both classes declare
//! exactly those fields in exactly that order before any of this is applied,
//! so a schema that writes them differently yields nothing rather than a
//! misread.
//!
//! What is *not* established here is what `m_layerFunction` means. Its value
//! is carried through as stored and marked as unknown (see
//! [`CompoundLayer::function`]); nothing in the corpus labels the enum, and
//! Revit's own IFC export writes an empty `Category` on every material
//! constituent, so there is no oracle for it yet. The core band
//! (`m_numShellLayersExt`/`Int`) and `m_structuralMaterialLayerIndex` are read
//! because they are positional, not enumerated.

use rvt_schema::{FieldType, Schema};

use crate::serial::{SerialObject, walk_record_collecting};

/// Class owning the layer list.
pub const COMPOUND_STRUCTURE_CLASS_NAME: &str = "CompoundStructure";
/// Class of one layer in that list.
pub const COMPOUND_STRUCTURE_LAYER_CLASS_NAME: &str = "CompoundStructureLayer";
/// Class every compound host object's type descends from.
pub const HOST_OBJECT_ATTRIBUTES_CLASS_NAME: &str = "HostObjAttr";

/// Revit's invalid element identifier.
const INVALID_ELEMENT_ID: i32 = -1;
/// `Integer32` values one inline `CompoundStructureLayer` contributes.
const LAYER_INTEGERS: usize = 5;
/// `Integer32` values `CompoundStructure` contributes after its layers:
/// `m_coarseScaleFillPatternElemId` and the six declared counts and indexes.
/// `m_coarseScaleFillColor` is an `Integer32Alternate` and is not collected.
const STRUCTURE_INTEGERS: usize = 7;
/// Largest layer count accepted from one structure. Revit's own limit is far
/// below this; the bound only keeps a misread object from claiming the file.
const MAX_LAYERS: usize = 256;

/// Declared properties of `CompoundStructure`, in order, as this reader
/// assumes them. Only the prefix that the reading depends on is checked: the
/// two trailing `m_segRefFaceKeys` collections may follow in any shape,
/// because they are read after everything this module takes.
const STRUCTURE_PROPERTIES: &[(&str, FieldType)] = &[
    ("m_oVertRegStructure", FieldType::Object),
    ("m_layers", FieldType::Object),
    ("m_coarseScaleFillPatternElemId", FieldType::Object),
    ("m_coarseScaleFillColor", FieldType::Integer32Alternate),
    ("m_endCap", FieldType::Integer32),
    ("m_openingWrapping", FieldType::Integer32),
    ("m_numShellLayersExt", FieldType::Integer32),
    ("m_numShellLayersInt", FieldType::Integer32),
    ("m_variableLayerIdx", FieldType::Integer32),
    ("m_structuralMaterialLayerIndex", FieldType::Integer32),
];

/// Declared properties of `CompoundStructureLayer`, in order. All seven are
/// checked: every one of them contributes to the pairing.
const LAYER_PROPERTIES: &[(&str, FieldType)] = &[
    ("m_layerWidth", FieldType::Float64),
    ("m_layerFunction", FieldType::Integer32),
    ("m_embeddingType", FieldType::Integer32),
    ("m_materialId", FieldType::Object),
    ("m_profileId", FieldType::Object),
    ("m_layerId", FieldType::Integer32),
    ("m_layerCapFlag", FieldType::Bool),
];

/// Dynamic class indexes this reader needs, resolved from a file's own schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompoundStructureClassIndexes {
    pub structure: u16,
    pub layer: u16,
}

impl CompoundStructureClassIndexes {
    /// Resolve the two classes and verify they declare what this module reads.
    ///
    /// `None` when either class is missing, when `CompoundStructure.m_layers`
    /// does not name `CompoundStructureLayer` as its static element class, or
    /// when either declaration list differs from what the pairing assumes. A
    /// schema that changed the layout is thereby not decoded at all, rather
    /// than decoded wrongly.
    #[must_use]
    pub fn detect(schema: &Schema) -> Option<Self> {
        let structure = schema.class_by_name(COMPOUND_STRUCTURE_CLASS_NAME)?;
        let layer = schema.class_by_name(COMPOUND_STRUCTURE_LAYER_CLASS_NAME)?;
        if !declares(structure.properties.as_slice(), STRUCTURE_PROPERTIES)
            || !declares_exactly(layer.properties.as_slice(), LAYER_PROPERTIES)
        {
            return None;
        }
        let layers = structure.properties.get(1)?;
        if layers.item_mode != 5 || layers.loading_mode != 0 {
            return None;
        }
        if layers.static_type.as_ref()?.index()? != layer.index {
            return None;
        }
        Some(Self {
            structure: structure.index,
            layer: layer.index,
        })
    }
}

/// One layer of a compound structure, as stored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompoundLayer {
    /// `m_layerWidth`, in Revit internal feet. Zero for a membrane layer.
    pub width_feet: f64,
    /// `m_layerFunction` as stored. Revit's meaning for the value is **not**
    /// established: nothing in the corpus labels the enum, so this is carried
    /// through as an opaque number and must not be rendered as a function name.
    pub function: i32,
    /// `m_embeddingType` as stored, likewise unlabelled.
    pub embedding_type: i32,
    /// `m_materialId`: the `MaterialElem` whose `Material.m_name` names this
    /// layer. `None` for Revit's invalid identifier - a layer with no material.
    pub material_id: Option<i32>,
    /// `m_profileId`, for a layer that follows a profile rather than a width.
    pub profile_id: Option<i32>,
    /// `m_layerId`: the type-local identifier Revit keeps a layer under.
    pub layer_id: i32,
    /// `m_layerCapFlag`.
    pub cap: bool,
}

/// The layer table one type carries.
#[derive(Clone, Debug, PartialEq)]
pub struct CompoundStructure {
    /// Offset of the structure object in the record body it was read from.
    pub offset: usize,
    /// The layers, in the order the type lists them.
    pub layers: Vec<CompoundLayer>,
    /// `m_coarseScaleFillPatternElemId`, the first scalar written after the
    /// layers. Kept because it is the evidence that the scalars are aligned:
    /// it is an element identifier or Revit's invalid one, never a small enum.
    pub coarse_scale_fill_pattern_id: Option<i32>,
    /// `m_endCap`.
    pub end_cap: i32,
    /// `m_openingWrapping`.
    pub opening_wrapping: i32,
    /// `m_numShellLayersExt`: how many leading layers sit outside the core.
    pub shell_layers_exterior: i32,
    /// `m_numShellLayersInt`: how many trailing layers sit inside the core.
    pub shell_layers_interior: i32,
    /// `m_variableLayerIdx`, or `None` when no layer is variable.
    pub variable_layer_index: Option<usize>,
    /// `m_structuralMaterialLayerIndex`, or `None` when none is marked.
    pub structural_layer_index: Option<usize>,
}

impl CompoundStructure {
    /// Read every compound structure a record's node stream holds.
    ///
    /// A type carries one; the vector is returned because nothing establishes
    /// that a record cannot hold more, and reporting all of them is honest
    /// where picking one would be a guess.
    #[must_use]
    pub fn from_record(
        schema: &Schema,
        class_index: u16,
        body: &[u8],
        classes: CompoundStructureClassIndexes,
    ) -> Vec<Self> {
        let (_walk, objects) = walk_record_collecting(schema, class_index, body);
        Self::from_objects(&objects, classes)
    }

    /// The reading half of [`CompoundStructure::from_record`], for a node
    /// stream that has already been walked.
    #[must_use]
    pub fn from_objects(
        objects: &[SerialObject],
        classes: CompoundStructureClassIndexes,
    ) -> Vec<Self> {
        objects
            .iter()
            .filter(|object| object.class_index == classes.structure)
            .filter_map(Self::read)
            .collect()
    }

    /// Total of every layer's width, in Revit internal feet.
    #[must_use]
    pub fn total_width_feet(&self) -> f64 {
        self.layers.iter().map(|layer| layer.width_feet).sum()
    }

    /// Split one walked `CompoundStructure` object into its layers and its own
    /// scalars.
    ///
    /// `None` when the collected values do not pair up as the declarations
    /// require, or when the counts and indexes the structure carries do not
    /// address its own layers. Both are rejections rather than repairs: a
    /// structure that fails them was not read, and saying so is the point.
    fn read(object: &SerialObject) -> Option<Self> {
        let count = object.numbers.len();
        if count == 0 || count > MAX_LAYERS {
            return None;
        }
        let layer_integers = count.checked_mul(LAYER_INTEGERS)?;
        if object.integers.len() < layer_integers.checked_add(STRUCTURE_INTEGERS)?
            || object.small_integers.len() < count
        {
            return None;
        }
        let mut layers = Vec::with_capacity(count);
        for index in 0..count {
            let width = object.numbers[index];
            if !width.is_finite() || width < 0.0 {
                return None;
            }
            let fields = object.integers.get(index * LAYER_INTEGERS..)?;
            layers.push(CompoundLayer {
                width_feet: width,
                function: *fields.first()?,
                embedding_type: *fields.get(1)?,
                material_id: element_id(*fields.get(2)?),
                profile_id: element_id(*fields.get(3)?),
                layer_id: *fields.get(4)?,
                cap: object.small_integers.get(index)? != &0,
            });
        }
        let scalars = object.integers.get(layer_integers..)?;
        let exterior = *scalars.get(3)?;
        let interior = *scalars.get(4)?;
        let variable = *scalars.get(5)?;
        let structural = *scalars.get(6)?;
        let total = i32::try_from(count).ok()?;
        if exterior < 0 || interior < 0 || exterior.checked_add(interior)? > total {
            return None;
        }
        Some(Self {
            offset: object.offset,
            layers,
            coarse_scale_fill_pattern_id: element_id(*scalars.first()?),
            end_cap: *scalars.get(1)?,
            opening_wrapping: *scalars.get(2)?,
            shell_layers_exterior: exterior,
            shell_layers_interior: interior,
            variable_layer_index: layer_index(variable, total),
            structural_layer_index: layer_index(structural, total),
        })
    }
}

/// A stored identifier, or `None` for Revit's invalid one.
const fn element_id(value: i32) -> Option<i32> {
    if value == INVALID_ELEMENT_ID {
        None
    } else {
        Some(value)
    }
}

/// A stored layer index, kept only when it addresses one of `total` layers.
fn layer_index(value: i32, total: i32) -> Option<usize> {
    (value >= 0 && value < total)
        .then(|| usize::try_from(value).ok())
        .flatten()
}

/// Whether `properties` opens with exactly `expected`, name and type in order.
fn declares(properties: &[rvt_schema::PropertyDefinition], expected: &[(&str, FieldType)]) -> bool {
    properties.len() >= expected.len()
        && properties
            .iter()
            .zip(expected)
            .all(|(declared, (name, field_type))| {
                declared.name == *name && declared.field_type == *field_type
            })
}

/// Whether `properties` is exactly `expected` and nothing further.
fn declares_exactly(
    properties: &[rvt_schema::PropertyDefinition],
    expected: &[(&str, FieldType)],
) -> bool {
    properties.len() == expected.len() && declares(properties, expected)
}

#[cfg(test)]
mod tests {
    use rvt_schema::{ClassDefinition, PropertyDefinition, TypeReference};

    use super::*;

    const IDENTIFIER: u16 = 12;
    const ELEMENT_ID: u16 = 13;
    const LAYER: u16 = 14;
    const STRUCTURE: u16 = 15;
    const OWNER: u16 = 16;

    fn property(
        name: &str,
        field_type: FieldType,
        loading_mode: u8,
        item_mode: i8,
    ) -> PropertyDefinition {
        PropertyDefinition {
            name: name.to_owned(),
            name_bytes: name.as_bytes().to_vec(),
            field_type,
            raw_modes: loading_mode,
            loading_mode,
            item_mode,
            unknown_word: 0,
            size: None,
            element: None,
            static_type: None,
            space_name_word: None,
            offset: 0,
        }
    }

    fn inline(name: &str, index: u16, class: &str, item_mode: i8) -> PropertyDefinition {
        let mut declared = property(name, FieldType::Object, 0x00, item_mode);
        declared.static_type = Some(TypeReference::Reference {
            index,
            name: class.to_owned(),
        });
        declared
    }

    fn class(index: u16, name: &str, properties: Vec<PropertyDefinition>) -> ClassDefinition {
        ClassDefinition {
            index,
            name: name.to_owned(),
            name_bytes: name.as_bytes().to_vec(),
            parent: TypeReference::None,
            version: 1,
            properties,
            guids: Vec::new(),
            unknown_word: 0,
            inline: false,
            offset: 0,
            end_offset: 0,
        }
    }

    /// `Identifier`, `ElementId`, the layer, the structure, and an owner whose
    /// single property references the structure - the shape `HostObjAttr` has.
    fn schema() -> Schema {
        Schema {
            classes: vec![
                class(
                    IDENTIFIER,
                    "Identifier",
                    vec![property("m_id", FieldType::Integer32, 0x00, 0)],
                ),
                class(
                    ELEMENT_ID,
                    "ElementId",
                    vec![inline("m_id", IDENTIFIER, "Identifier", 0)],
                ),
                class(
                    LAYER,
                    COMPOUND_STRUCTURE_LAYER_CLASS_NAME,
                    vec![
                        property("m_layerWidth", FieldType::Float64, 0x00, 0),
                        property("m_layerFunction", FieldType::Integer32, 0x00, 0),
                        property("m_embeddingType", FieldType::Integer32, 0x00, 0),
                        inline("m_materialId", ELEMENT_ID, "ElementId", 0),
                        inline("m_profileId", ELEMENT_ID, "ElementId", 0),
                        property("m_layerId", FieldType::Integer32, 0x00, 0),
                        property("m_layerCapFlag", FieldType::Bool, 0x00, 0),
                    ],
                ),
                class(
                    STRUCTURE,
                    COMPOUND_STRUCTURE_CLASS_NAME,
                    vec![
                        property("m_oVertRegStructure", FieldType::Object, 0x01, 0),
                        inline("m_layers", LAYER, COMPOUND_STRUCTURE_LAYER_CLASS_NAME, 5),
                        inline("m_coarseScaleFillPatternElemId", ELEMENT_ID, "ElementId", 0),
                        property(
                            "m_coarseScaleFillColor",
                            FieldType::Integer32Alternate,
                            0x00,
                            0,
                        ),
                        property("m_endCap", FieldType::Integer32, 0x00, 0),
                        property("m_openingWrapping", FieldType::Integer32, 0x00, 0),
                        property("m_numShellLayersExt", FieldType::Integer32, 0x00, 0),
                        property("m_numShellLayersInt", FieldType::Integer32, 0x00, 0),
                        property("m_variableLayerIdx", FieldType::Integer32, 0x00, 0),
                        property(
                            "m_structuralMaterialLayerIndex",
                            FieldType::Integer32,
                            0x00,
                            0,
                        ),
                    ],
                ),
                class(
                    OWNER,
                    "Owner",
                    vec![property("m_pCompoundStructure", FieldType::Object, 0x01, 0)],
                ),
            ],
            top_level_class_count: 5,
            property_count: 0,
            parsed_property_count: 0,
            consumed_bytes: 0,
            trailing_bytes: Vec::new(),
            unresolved_references: Vec::new(),
            inline_index_mismatches: Vec::new(),
        }
    }

    /// One layer as the file writes it: width, function, embedding type, two
    /// element identifiers, layer id, cap flag.
    fn layer_bytes(width_feet: f64, function: i32, material: i32, layer_id: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&width_feet.to_le_bytes());
        bytes.extend_from_slice(&function.to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&material.to_le_bytes());
        bytes.extend_from_slice(&INVALID_ELEMENT_ID.to_le_bytes());
        bytes.extend_from_slice(&layer_id.to_le_bytes());
        bytes.push(0);
        bytes
    }

    /// A record of `Owner`: the reference to the structure, then the structure
    /// object the reference names, then the record's length trailer.
    fn record(layers: &[Vec<u8>], exterior: i32, interior: i32, structural: i32) -> Vec<u8> {
        let mut body = Vec::new();
        // `Owner.m_pCompoundStructure`: a live reference is an identifier and
        // the class index of what it names. This one opens the record body, so
        // its identifier takes the narrow two bytes.
        body.extend_from_slice(&1_u16.to_le_bytes());
        body.extend_from_slice(&STRUCTURE.to_le_bytes());
        // The structure object: a null reference for `m_oVertRegStructure`,
        // then the counted layers, then its own scalars.
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&u32::try_from(layers.len()).unwrap().to_le_bytes());
        for layer in layers {
            body.extend_from_slice(layer);
        }
        body.extend_from_slice(&INVALID_ELEMENT_ID.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&0_i32.to_le_bytes());
        body.extend_from_slice(&exterior.to_le_bytes());
        body.extend_from_slice(&interior.to_le_bytes());
        body.extend_from_slice(&(-1_i32).to_le_bytes());
        body.extend_from_slice(&structural.to_le_bytes());
        let length = u32::try_from(body.len() + 4).unwrap();
        body.extend_from_slice(&length.to_le_bytes());
        body
    }

    #[test]
    fn detects_the_declared_classes() {
        let schema = schema();
        let classes = CompoundStructureClassIndexes::detect(&schema).unwrap();
        assert_eq!(classes.structure, STRUCTURE);
        assert_eq!(classes.layer, LAYER);
    }

    #[test]
    fn rejects_a_schema_that_declares_the_layer_differently() {
        let mut schema = schema();
        let layer = schema
            .classes
            .iter_mut()
            .find(|class| class.name == COMPOUND_STRUCTURE_LAYER_CLASS_NAME)
            .unwrap();
        layer.properties.swap(0, 1);
        assert_eq!(CompoundStructureClassIndexes::detect(&schema), None);
    }

    #[test]
    fn reads_the_layers_in_declaration_order() {
        let schema = schema();
        let classes = CompoundStructureClassIndexes::detect(&schema).unwrap();
        let body = record(
            &[
                layer_bytes(0.041_010_498_687_664_04, 4, 700, 11),
                layer_bytes(0.656_167_979_002_624_7, 1, 701, 12),
                layer_bytes(0.041_010_498_687_664_04, 4, 700, 13),
            ],
            1,
            1,
            1,
        );
        let read = CompoundStructure::from_record(&schema, OWNER, &body, classes);
        assert_eq!(read.len(), 1);
        let structure = &read[0];
        assert_eq!(structure.layers.len(), 3);
        assert_eq!(
            structure
                .layers
                .iter()
                .map(|layer| layer.material_id)
                .collect::<Vec<_>>(),
            vec![Some(700), Some(701), Some(700)]
        );
        assert_eq!(
            structure
                .layers
                .iter()
                .map(|layer| layer.layer_id)
                .collect::<Vec<_>>(),
            vec![11, 12, 13]
        );
        assert_eq!(structure.layers[1].function, 1);
        assert_eq!(structure.layers[0].profile_id, None);
        assert_eq!(structure.shell_layers_exterior, 1);
        assert_eq!(structure.shell_layers_interior, 1);
        assert_eq!(structure.structural_layer_index, Some(1));
        assert_eq!(structure.variable_layer_index, None);
        // 12.5 + 200 + 12.5 mm, in Revit internal feet.
        assert!((structure.total_width_feet() * 304.8 - 225.0).abs() < 1.0e-9);
    }

    #[test]
    fn rejects_a_core_band_wider_than_the_layer_list() {
        let schema = schema();
        let classes = CompoundStructureClassIndexes::detect(&schema).unwrap();
        let body = record(&[layer_bytes(0.5, 1, 700, 11)], 1, 1, 0);
        assert!(CompoundStructure::from_record(&schema, OWNER, &body, classes).is_empty());
    }

    #[test]
    fn drops_an_out_of_range_structural_layer_index() {
        let schema = schema();
        let classes = CompoundStructureClassIndexes::detect(&schema).unwrap();
        let body = record(&[layer_bytes(0.5, 1, 700, 11)], 0, 0, 7);
        let read = CompoundStructure::from_record(&schema, OWNER, &body, classes);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].structural_layer_index, None);
    }
}

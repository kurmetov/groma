//! Turning an IFC entity table into the format-neutral model the scene writer
//! and the JSON export both consume.
//!
//! What an element is here is settled by the file: a product is an entity that
//! carries an `IfcProductDefinitionShape` or that a spatial containment names,
//! its type is the entity it was written as, and its storey is whichever one
//! contains it - directly or through whatever it is aggregated into. Nothing
//! is inferred from a name.

use std::collections::{HashMap, HashSet};

use bim_core::{
    BimCategory, BimElement, BimElementId, BimElementType, BimExternalId, BimGeometry, BimLevel,
    BimMaterial, BimMaterialLayer, BimMaterialLayerSet, BimModel, BimNumber, BimPlacement,
    BimProperty, BimPropertyValue, BimSource, BimUnit,
};

use crate::curve::Sampler;
use crate::place::{Affine, Placements, normalize};
use crate::solid::{Builder, metres, point3};
use crate::step::{Entity, Parsed, Value};
use crate::units::Units;

/// How finely to read the curves a file states.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The furthest a sampled chord may sit from the curve it replaces, in
    /// metres.
    pub chord_tolerance: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chord_tolerance: 0.004,
        }
    }
}

/// A converted model, with what the conversion did not read alongside it.
pub struct Import {
    pub model: BimModel,
    pub read: Read,
}

/// What one conversion saw, so a caller can say what it did and did not draw.
#[derive(Clone, Copy, Debug, Default)]
pub struct Read {
    pub products: usize,
    pub with_geometry: usize,
    /// Products whose representation held nothing this reads.
    pub without_geometry: usize,
    /// Representation items whose form this does not read.
    pub unread_items: usize,
    /// Items drawn without a cut the file states.
    pub approximated_items: usize,
    /// Object placements this could not resolve.
    pub unread_placements: usize,
    /// Openings, which are voids rather than products, and are not drawn.
    pub openings: usize,
    /// Whether the file declared the length unit its numbers are in.
    pub stated_length_unit: bool,
}

/// The spatial entities a model is divided into rather than filled with.
const SPATIAL: &[&str] = &[
    "IFCPROJECT",
    "IFCSITE",
    "IFCBUILDING",
    "IFCBUILDINGSTOREY",
    "IFCSPATIALZONE",
    "IFCEXTERNALSPATIALELEMENT",
];

/// Entities that are products but are not things to draw: a void, a note, or
/// a placeholder for something absent.
const NOT_DRAWN: &[&str] = &[
    "IFCOPENINGELEMENT",
    "IFCOPENINGSTANDARDCASE",
    "IFCVOIDINGFEATURE",
    "IFCANNOTATION",
    "IFCGRID",
    "IFCVIRTUALELEMENT",
];

/// Read a parsed file into the canonical model.
#[must_use]
pub fn convert(parsed: &Parsed, options: &Options) -> Import {
    let units = Units::read(parsed);
    let mut read = Read {
        stated_length_unit: units.stated_length,
        ..Read::default()
    };
    let mut placements = Placements::new(parsed, units);
    let index = Index::build(parsed);
    let (levels, level_of_storey) = levels(parsed, &index, units);

    let mut elements = Vec::new();
    for (id, entity) in products(parsed, &index) {
        if NOT_DRAWN.contains(&entity.type_name.as_str()) {
            read.openings += 1;
            continue;
        }
        read.products += 1;
        let world = entity
            .attribute(5)
            .and_then(Value::as_reference)
            .and_then(|placement| placements.world(placement));
        let geometry = geometry(
            parsed,
            entity,
            world.unwrap_or_default(),
            units,
            *options,
            &mut read,
        );
        if geometry.is_some() {
            read.with_geometry += 1;
        } else {
            read.without_geometry += 1;
        }
        let type_id = index.type_of.get(&id).copied();
        elements.push(BimElement {
            id: identity(parsed, id, entity),
            element_type: element_type(&entity.type_name),
            class_name: Some(entity.type_name.clone()),
            name: text(entity.attribute(2)),
            // A space is named by its number and described by its name, which
            // is what `LongName` carries for every spatial element.
            long_name: entity
                .type_name
                .starts_with("IFCSPACE")
                .then(|| text(entity.attribute(7)))
                .flatten(),
            category: None,
            level_id: index
                .storey_of(id)
                .and_then(|storey| level_of_storey.get(&storey).cloned()),
            type_id: type_id
                .and_then(|type_id| Some((type_id, parsed.get(type_id)?)))
                .map(|(type_id, entity)| identity(parsed, type_id, entity)),
            placement: world.map(placement),
            geometry,
            properties: properties(parsed, &index, id, units),
            type_properties: type_id
                .map(|type_id| properties(parsed, &index, type_id, units))
                .unwrap_or_default(),
            material_layers: material_layers(parsed, &index, id, type_id, units),
        });
    }
    read.unread_placements = placements.unread;

    Import {
        model: BimModel {
            source: source(parsed),
            elements,
            levels,
            relations: Vec::new(),
        },
        read,
    }
}

/// The relations a conversion reads, gathered once rather than searched for
/// per element.
struct Index {
    /// What an object is inside, whether a storey contains it or another
    /// element is made of it.
    container: HashMap<u64, u64>,
    /// Objects a spatial containment names, whatever shape they carry.
    contained: HashSet<u64>,
    storeys: HashSet<u64>,
    /// Property and quantity sets, by the object they were stated for.
    properties: HashMap<u64, Vec<u64>>,
    type_of: HashMap<u64, u64>,
    material_of: HashMap<u64, u64>,
}

impl Index {
    fn build(parsed: &Parsed) -> Self {
        let mut index = Self {
            container: HashMap::new(),
            contained: HashSet::new(),
            storeys: parsed
                .of_type("IFCBUILDINGSTOREY")
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            properties: HashMap::new(),
            type_of: HashMap::new(),
            material_of: HashMap::new(),
        };
        for (_, relation) in parsed.of_type("IFCRELCONTAINEDINSPATIALSTRUCTURE") {
            let Some(structure) = relation.attribute(5).and_then(Value::as_reference) else {
                continue;
            };
            for related in references(relation.attribute(4)) {
                index.container.insert(related, structure);
                index.contained.insert(related);
            }
        }
        // An aggregate puts its parts inside itself: a storey holds its
        // spaces this way, and an assembly holds its members.
        for (_, relation) in parsed.of_type("IFCRELAGGREGATES") {
            let Some(whole) = relation.attribute(4).and_then(Value::as_reference) else {
                continue;
            };
            for part in references(relation.attribute(5)) {
                index.container.entry(part).or_insert(whole);
            }
        }
        for (_, relation) in parsed.of_type("IFCRELDEFINESBYPROPERTIES") {
            let Some(definition) = relation.attribute(5).and_then(Value::as_reference) else {
                continue;
            };
            for object in references(relation.attribute(4)) {
                index.properties.entry(object).or_default().push(definition);
            }
        }
        for (_, relation) in parsed.of_type("IFCRELDEFINESBYTYPE") {
            let Some(kind) = relation.attribute(5).and_then(Value::as_reference) else {
                continue;
            };
            for object in references(relation.attribute(4)) {
                index.type_of.insert(object, kind);
            }
        }
        for (_, relation) in parsed.of_type("IFCRELASSOCIATESMATERIAL") {
            let Some(material) = relation.attribute(5).and_then(Value::as_reference) else {
                continue;
            };
            for object in references(relation.attribute(4)) {
                index.material_of.insert(object, material);
            }
        }
        index
    }

    /// The storey an object is on, following whatever it is part of until one
    /// is found.
    fn storey_of(&self, id: u64) -> Option<u64> {
        let mut at = id;
        for _ in 0..32 {
            let parent = *self.container.get(&at)?;
            if self.storeys.contains(&parent) {
                return Some(parent);
            }
            at = parent;
        }
        None
    }
}

fn references(value: Option<&Value>) -> Vec<u64> {
    value
        .and_then(Value::as_list)
        .map(|members| members.iter().filter_map(Value::as_reference).collect())
        .unwrap_or_default()
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_text)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// An entity's identity: the globally unique id every rooted entity carries,
/// falling back to its instance number where a file leaves one out.
fn identity(parsed: &Parsed, id: u64, entity: &Entity) -> BimElementId {
    let _ = parsed;
    BimElementId(text(entity.attribute(0)).unwrap_or_else(|| format!("#{id}")))
}

fn source(parsed: &Parsed) -> Option<BimSource> {
    parsed.application.clone().map(|application| BimSource {
        application,
        release: parsed.schema.clone(),
    })
}

/// Every product a file states, in file order.
fn products<'parsed>(parsed: &'parsed Parsed, index: &Index) -> Vec<(u64, &'parsed Entity)> {
    let mut found: Vec<(u64, &Entity)> = parsed
        .entities
        .iter()
        .filter(|(id, entity)| {
            if SPATIAL.contains(&entity.type_name.as_str()) {
                return false;
            }
            // `Representation` is the seventh attribute of every product, and
            // only a product carries an `IfcProductDefinitionShape` there.
            let shaped = parsed
                .follow(entity.attribute(6))
                .is_some_and(|shape| shape.type_name == "IFCPRODUCTDEFINITIONSHAPE");
            shaped || index.contained.contains(id)
        })
        .map(|(id, entity)| (*id, entity))
        .collect();
    found.sort_unstable_by_key(|(id, _)| *id);
    found
}

fn levels(
    parsed: &Parsed,
    index: &Index,
    units: Units,
) -> (Vec<BimLevel>, HashMap<u64, BimElementId>) {
    let _ = index;
    let mut levels = Vec::new();
    let mut by_entity = HashMap::new();
    for (id, storey) in parsed.of_type("IFCBUILDINGSTOREY") {
        let level_id = identity(parsed, id, storey);
        by_entity.insert(id, level_id.clone());
        levels.push(BimLevel {
            id: level_id,
            name: text(storey.attribute(2)).or_else(|| text(storey.attribute(7))),
            // `Elevation` is the tenth attribute, in the file's length unit.
            elevation: storey
                .attribute(9)
                .and_then(Value::as_number)
                .map(|value| BimNumber {
                    value: value * units.length,
                    unit: Some(metres()),
                }),
        });
    }
    levels.sort_by(|left, right| {
        let height = |level: &BimLevel| {
            level
                .elevation
                .as_ref()
                .map_or(f64::NEG_INFINITY, |number| number.value)
        };
        height(left)
            .partial_cmp(&height(right))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (levels, by_entity)
}

fn placement(world: Affine) -> BimPlacement {
    BimPlacement {
        origin: point3(world.origin),
        reference_direction: normalize(world.basis[0]).unwrap_or([1.0, 0.0, 0.0]),
        axis: normalize(world.basis[2]).unwrap_or([0.0, 0.0, 1.0]),
    }
}

/// The body a product's shape states, in world coordinates.
fn geometry(
    parsed: &Parsed,
    entity: &Entity,
    world: Affine,
    units: Units,
    options: Options,
    read: &mut Read,
) -> Option<BimGeometry> {
    let shape = parsed.follow(entity.attribute(6))?;
    if shape.type_name != "IFCPRODUCTDEFINITIONSHAPE" {
        return None;
    }
    let mut builder = Builder::new(Sampler {
        parsed,
        units,
        tolerance: options.chord_tolerance,
    });
    let mut extent = None;
    for value in shape
        .attribute(2)
        .and_then(Value::as_list)
        .unwrap_or_default()
    {
        let Some(representation) = parsed.follow(Some(value)) else {
            continue;
        };
        match representation
            .attribute(1)
            .and_then(Value::as_text)
            .unwrap_or_default()
        {
            // A body is the shape itself. Everything else a file carries
            // beside it - an axis, a footprint, a survey point - describes the
            // element without bounding it, and drawing it would put lines in
            // the model that are not its surface.
            "Body" | "Facetation" => builder.representation(representation, &world, 0),
            "Box" => extent = Some(representation),
            _ => {}
        }
    }
    // A stated extent stands in only where the body held nothing this reads.
    if builder.bodies.is_empty() {
        if let Some(representation) = extent {
            builder.representation(representation, &world, 0);
        }
    }
    read.unread_items += builder.bodies.unread_items;
    read.approximated_items += builder.bodies.approximated;
    builder.bodies.finish(options.chord_tolerance)
}

/// The IFC entity an element was written as, read back into the model's own
/// type.
///
/// This is the inverse of what `ifc-export` writes, plus the standard-case
/// subtypes a file may state instead. An entity with no counterpart stays
/// `Unknown` rather than being folded into a neighbouring one: the entity
/// name is kept on the element, so nothing is lost by declining to guess.
#[must_use]
pub fn element_type(entity_name: &str) -> BimElementType {
    // A `...STANDARDCASE` or `...ELEMENTEDCASE` is the same product under a
    // constraint on how it is built.
    let base = entity_name
        .strip_suffix("STANDARDCASE")
        .or_else(|| entity_name.strip_suffix("ELEMENTEDCASE"))
        .unwrap_or(entity_name);
    match base {
        "IFCPIPESEGMENT" => BimElementType::PipeSegment,
        "IFCPIPEFITTING" => BimElementType::PipeFitting,
        "IFCSANITARYTERMINAL" => BimElementType::SanitaryTerminal,
        "IFCAIRTERMINAL" => BimElementType::AirTerminal,
        "IFCFIRESUPPRESSIONTERMINAL" => BimElementType::FireSuppressionTerminal,
        "IFCALARM" => BimElementType::Alarm,
        "IFCCABLECARRIERFITTING" => BimElementType::CableCarrierFitting,
        "IFCDUCTSEGMENT" => BimElementType::DuctSegment,
        "IFCCABLECARRIERSEGMENT" => BimElementType::CableCarrierSegment,
        "IFCDISTRIBUTIONELEMENT" => BimElementType::DistributionElement,
        "IFCDISTRIBUTIONFLOWELEMENT" => BimElementType::DistributionFlowElement,
        "IFCWALL" => BimElementType::Wall,
        "IFCSLAB" => BimElementType::Slab,
        "IFCROOF" => BimElementType::Roof,
        "IFCSTAIR" => BimElementType::Stair,
        "IFCSTAIRFLIGHT" => BimElementType::StairFlight,
        "IFCCURTAINWALL" => BimElementType::CurtainWall,
        "IFCRAILING" => BimElementType::Railing,
        "IFCCOLUMN" => BimElementType::Column,
        "IFCMEMBER" => BimElementType::Member,
        "IFCPLATE" => BimElementType::Plate,
        "IFCWINDOW" => BimElementType::Window,
        "IFCDOOR" => BimElementType::Door,
        "IFCSPACE" => BimElementType::Space,
        _ => BimElementType::Unknown,
    }
}

fn properties(parsed: &Parsed, index: &Index, id: u64, units: Units) -> Vec<BimProperty> {
    let mut out = Vec::new();
    for definition in index.properties.get(&id).into_iter().flatten() {
        let Some(entity) = parsed.get(*definition) else {
            continue;
        };
        let set_name = text(entity.attribute(2));
        match entity.type_name.as_str() {
            "IFCPROPERTYSET" => {
                for value in references(entity.attribute(4)) {
                    push_property(parsed, value, set_name.as_deref(), units, &mut out, 0);
                }
            }
            "IFCELEMENTQUANTITY" => {
                for value in references(entity.attribute(5)) {
                    push_quantity(parsed, value, set_name.as_deref(), units, &mut out, 0);
                }
            }
            _ => {}
        }
    }
    out
}

/// Where a property came from, kept so that two sets stating the same name
/// stay distinguishable.
fn from_set(set_name: Option<&str>) -> Option<BimExternalId> {
    set_name.map(|name| BimExternalId {
        system: "ifc.propertySet".to_owned(),
        value: name.to_owned(),
    })
}

fn push_property(
    parsed: &Parsed,
    id: u64,
    set_name: Option<&str>,
    units: Units,
    out: &mut Vec<BimProperty>,
    depth: u8,
) {
    if depth > 4 {
        return;
    }
    let Some(entity) = parsed.get(id) else {
        return;
    };
    let Some(name) = text(entity.attribute(0)) else {
        return;
    };
    let value = match entity.type_name.as_str() {
        "IFCPROPERTYSINGLEVALUE" => entity.attribute(2).and_then(|value| measure(value, units)),
        // An enumerated or list-valued property is stated as several values;
        // they are joined rather than one of them picked.
        "IFCPROPERTYENUMERATEDVALUE" | "IFCPROPERTYLISTVALUE" => {
            entity.attribute(2).and_then(Value::as_list).map(|values| {
                BimPropertyValue::Text(
                    values
                        .iter()
                        .filter_map(|value| match measure(value, units) {
                            Some(BimPropertyValue::Text(text)) => Some(text),
                            Some(BimPropertyValue::Number(number)) => {
                                Some(number.value.to_string())
                            }
                            Some(BimPropertyValue::Integer(number)) => Some(number.to_string()),
                            Some(BimPropertyValue::Bool(flag)) => Some(flag.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<String>>()
                        .join(", "),
                )
            })
        }
        "IFCCOMPLEXPROPERTY" => {
            for member in references(entity.attribute(3)) {
                push_property(parsed, member, set_name, units, out, depth + 1);
            }
            return;
        }
        _ => None,
    };
    if let Some(value) = value {
        out.push(BimProperty {
            id: from_set(set_name),
            name,
            specification: None,
            value,
        });
    }
}

fn push_quantity(
    parsed: &Parsed,
    id: u64,
    set_name: Option<&str>,
    units: Units,
    out: &mut Vec<BimProperty>,
    depth: u8,
) {
    if depth > 4 {
        return;
    }
    let Some(entity) = parsed.get(id) else {
        return;
    };
    if entity.type_name == "IFCPHYSICALCOMPLEXQUANTITY" {
        for member in references(entity.attribute(2)) {
            push_quantity(parsed, member, set_name, units, out, depth + 1);
        }
        return;
    }
    let Some(name) = text(entity.attribute(0)) else {
        return;
    };
    let Some(number) = entity.attribute(2).and_then(Value::as_number) else {
        return;
    };
    // A quantity's dimension is its entity, and each is stated in the unit the
    // file assigned to that dimension. A length is carried into metres like
    // every other length here; an area and a volume are already the SI ones a
    // unit assignment may not scale.
    let (value, unit) = match entity.type_name.as_str() {
        "IFCQUANTITYLENGTH" => (number * units.length, Some(metres())),
        "IFCQUANTITYAREA" => (
            number,
            Some(BimUnit {
                id: "autodesk.unit.unit:squareMeters-1.0.1".to_owned(),
                name: "Square meters".to_owned(),
            }),
        ),
        "IFCQUANTITYVOLUME" => (
            number,
            Some(BimUnit {
                id: "autodesk.unit.unit:cubicMeters-1.0.1".to_owned(),
                name: "Cubic meters".to_owned(),
            }),
        ),
        "IFCQUANTITYWEIGHT" => (
            number,
            Some(BimUnit {
                id: "autodesk.unit.unit:kilograms-1.0.0".to_owned(),
                name: "Kilograms".to_owned(),
            }),
        ),
        "IFCQUANTITYCOUNT" => (number, None),
        _ => return,
    };
    out.push(BimProperty {
        id: from_set(set_name),
        name,
        specification: None,
        value: BimPropertyValue::Number(BimNumber { value, unit }),
    });
}

/// One property value, with a length carried into metres and everything else
/// left as the file wrote it.
fn measure(value: &Value, units: Units) -> Option<BimPropertyValue> {
    match value {
        Value::Text(text) => Some(BimPropertyValue::Text(text.clone())),
        Value::Integer(number) => Some(BimPropertyValue::Integer(*number)),
        Value::Real(number) => Some(BimPropertyValue::Number(BimNumber {
            value: *number,
            unit: None,
        })),
        Value::Enumeration(text) => match text.as_str() {
            "T" | "TRUE" => Some(BimPropertyValue::Bool(true)),
            "F" | "FALSE" => Some(BimPropertyValue::Bool(false)),
            "U" | "UNKNOWN" => None,
            other => Some(BimPropertyValue::Text(other.to_owned())),
        },
        Value::Typed(kind, inner) => {
            let inner_value = measure(inner, units)?;
            // The measure types that are lengths are the ones this rescales;
            // a count, a ratio or a label passes through as written.
            if matches!(
                kind.as_str(),
                "IFCLENGTHMEASURE" | "IFCPOSITIVELENGTHMEASURE" | "IFCNONNEGATIVELENGTHMEASURE"
            ) {
                if let BimPropertyValue::Number(number) = inner_value {
                    return Some(BimPropertyValue::Number(BimNumber {
                        value: number.value * units.length,
                        unit: Some(metres()),
                    }));
                }
            }
            Some(inner_value)
        }
        _ => None,
    }
}

/// The layered build-up a file associates with an element, or with its type.
fn material_layers(
    parsed: &Parsed,
    index: &Index,
    id: u64,
    type_id: Option<u64>,
    units: Units,
) -> Option<BimMaterialLayerSet> {
    let (source_type_id, association) = index
        .material_of
        .get(&id)
        .map(|material| (None, *material))
        .or_else(|| {
            type_id.and_then(|type_id| {
                index
                    .material_of
                    .get(&type_id)
                    .map(|material| (Some(type_id), *material))
            })
        })?;
    let mut entity = parsed.get(association)?;
    // A usage points at the set it places; the set itself is what carries the
    // layers.
    if entity.type_name == "IFCMATERIALLAYERSETUSAGE" {
        entity = parsed.follow(entity.attribute(0))?;
    }
    if entity.type_name != "IFCMATERIALLAYERSET" {
        return None;
    }
    let mut layers = Vec::new();
    for member in references(entity.attribute(0)) {
        let Some(layer) = parsed.get(member) else {
            continue;
        };
        let Some(thickness) = layer.attribute(1).and_then(Value::as_number) else {
            continue;
        };
        // `Category` is the schema's own word for what a layer is doing in the
        // build-up; `LoadBearing` is the one it uses for the structural core.
        let category = layer.attribute(5).and_then(Value::as_text).unwrap_or("");
        let structural = category.eq_ignore_ascii_case("LoadBearing");
        layers.push(BimMaterialLayer {
            material: parsed
                .follow(layer.attribute(0))
                .map(|material| BimMaterial {
                    id: None,
                    name: text(material.attribute(0)),
                }),
            thickness: BimNumber {
                value: thickness * units.length,
                unit: Some(metres()),
            },
            is_core: structural,
            is_structural: structural,
            // IFC states a layer's part in words, not as a code, so there is
            // no number here to carry.
            source_function: None,
        });
    }
    (!layers.is_empty()).then(|| BimMaterialLayerSet {
        source_type_id: source_type_id.and_then(|type_id| {
            parsed
                .get(type_id)
                .map(|entity| identity(parsed, type_id, entity))
        }),
        name: text(entity.attribute(1)),
        layers,
    })
}

/// Kept so that a caller can name the category an element carries; IFC states
/// none, and one is not invented here.
#[must_use]
pub fn category(_entity: &Entity) -> Option<BimCategory> {
    None
}

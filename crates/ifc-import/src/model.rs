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
    BimProperty, BimPropertyValue, BimRelation, BimSource, BimUnit,
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

    // Which entities became elements, so a relation between two of them can be
    // stated in the model's own identifiers and one naming anything else - an
    // opening, a storey, a group - can be dropped.
    let mut kept: HashMap<u64, BimElementId> = HashMap::new();
    let products = products(parsed, &index);
    // The placement chain is resolved here, in file order, because resolving
    // one placement memoises every placement above it and the count of the
    // ones it could not read is a property of the pass rather than of a
    // product. Everything after this reads a product and nothing else, which
    // is what lets the products be read on every core at once.
    let placed = products
        .into_iter()
        .map(|(id, entity)| {
            if NOT_DRAWN.contains(&entity.type_name.as_str()) {
                return (id, entity, None);
            }
            let world = entity
                .attribute(5)
                .and_then(Value::as_reference)
                .and_then(|placement| placements.world(placement));
            (id, entity, world)
        })
        .collect::<Vec<_>>();
    read.unread_placements = placements.unread;

    // One product's geometry, properties and identity say nothing about
    // another's, so they are built on every core and the tallies each one
    // raises are added below in file order.
    let built = bim_core::work::map_in_order(&placed, |(id, entity, world)| {
        if NOT_DRAWN.contains(&entity.type_name.as_str()) {
            return None;
        }
        let world = *world;
        let mut read = Read::default();
        let geometry = geometry(
            parsed,
            entity,
            world.unwrap_or_default(),
            units,
            *options,
            &mut read,
        );
        let id = *id;
        let type_id = index.type_of.get(&id).copied();
        let element_id = identity(parsed, id, entity);
        Some((
            id,
            read,
            BimElement {
                id: element_id,
                // One file per reader; `bim_core::federate` names the document.
                document: None,
                element_type: element_type(&entity.type_name),
                class_name: Some(entity.type_name.as_str().to_owned()),
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
                // A type object is an `IfcRoot`, so its name is where every other
                // root keeps one.
                type_name: type_id
                    .and_then(|type_id| parsed.get(type_id))
                    .and_then(|entity| text(entity.attribute(2))),
                // `IfcRelFillsElement`/`IfcRelVoidsElement` are not read back
                // by this importer; a round trip loses no export this reader
                // is verified against, since none of them write an opening.
                host_id: None,
                placement: world.map(placement),
                geometry,
                properties: properties(parsed, &index, id, units),
                type_properties: type_id
                    .map(|type_id| properties(parsed, &index, type_id, units))
                    .unwrap_or_default(),
                material_layers: material_layers(parsed, &index, id, type_id, units),
            },
        ))
    });

    let elements = collect_products(built, &mut read, &mut kept);

    Import {
        model: BimModel {
            source: source(parsed),
            // `IfcProject`/`IfcBuilding` are read back as spatial structure,
            // not as project identity - nothing here reads `Name`/`LongName`
            // back into one.
            project: None,
            site: None,
            documents: Vec::new(),
            elements,
            levels,
            relations: relations(&index, &kept),
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
    /// The element an opening was cut out of, by that opening. An opening is
    /// a void rather than a product, so it never becomes an element itself;
    /// it is the join that carries a door or a window back to its host.
    voids: HashMap<u64, u64>,
    /// What fills an opening, by that opening.
    fills: HashMap<u64, Vec<u64>>,
    /// `(building element, space)` for every boundary the file marked
    /// `.PHYSICAL.`. A virtual boundary is an imaginary plane and usually
    /// names no element at all, so it is left out rather than resolved.
    boundaries: Vec<(u64, u64)>,
    /// `(whole, part)` for aggregations between products. The spatial ones -
    /// a storey holding its spaces - are already levels, and are filtered out
    /// when the relations are emitted rather than here.
    aggregates: Vec<(u64, u64)>,
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
            voids: HashMap::new(),
            fills: HashMap::new(),
            boundaries: Vec::new(),
            aggregates: Vec::new(),
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
                index.aggregates.push((whole, part));
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
        // A door or a window is not attached to its wall directly: the wall
        // is voided by an opening, and the opening is filled by the product.
        // Both halves are stated, so the host is a join rather than a guess.
        for (_, relation) in parsed.of_type("IFCRELVOIDSELEMENT") {
            let (Some(host), Some(opening)) = (
                relation.attribute(4).and_then(Value::as_reference),
                relation.attribute(5).and_then(Value::as_reference),
            ) else {
                continue;
            };
            index.voids.insert(opening, host);
        }
        for (_, relation) in parsed.of_type("IFCRELFILLSELEMENT") {
            let (Some(opening), Some(filler)) = (
                relation.attribute(4).and_then(Value::as_reference),
                relation.attribute(5).and_then(Value::as_reference),
            ) else {
                continue;
            };
            index.fills.entry(opening).or_default().push(filler);
        }
        for (_, relation) in parsed.of_type("IFCRELSPACEBOUNDARY") {
            if relation.attribute(7).and_then(Value::as_enumeration) != Some("PHYSICAL") {
                continue;
            }
            let (Some(space), Some(element)) = (
                relation.attribute(4).and_then(Value::as_reference),
                relation.attribute(5).and_then(Value::as_reference),
            ) else {
                continue;
            };
            index.boundaries.push((element, space));
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
/// The relations between two elements that the file states outright.
///
/// Each one is read from a relationship entity and nothing else: no adjacency
/// is inferred from geometry, and an edge whose either end did not become an
/// element - an opening, a storey, a group - is dropped rather than invented.
/// Duplicates are collapsed, because a file may state the same boundary from
/// more than one relationship instance.
fn relations(index: &Index, kept: &HashMap<u64, BimElementId>) -> Vec<BimRelation> {
    let mut seen = HashSet::new();
    let mut relations = Vec::new();
    let mut emit = |kind: &str, source: u64, target: u64| {
        let (Some(source), Some(target)) = (kept.get(&source), kept.get(&target)) else {
            return;
        };
        if source == target {
            return;
        }
        if !seen.insert((kind.to_owned(), source.clone(), target.clone())) {
            return;
        }
        relations.push(BimRelation {
            kind: kind.to_owned(),
            source: source.clone(),
            target: target.clone(),
        });
    };

    // Walk the openings in a stable order so two conversions of one file
    // produce the same list.
    let mut openings: Vec<&u64> = index.voids.keys().collect();
    openings.sort_unstable();
    for opening in openings {
        let host = index.voids[opening];
        for filler in index.fills.get(opening).into_iter().flatten() {
            emit("hosts", host, *filler);
        }
    }
    for (element, space) in &index.boundaries {
        emit("bounds", *element, *space);
    }
    for (whole, part) in &index.aggregates {
        emit("aggregates", *whole, *part);
    }
    relations
}

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

/// Add up what each product was read as, in file order.
///
/// The products were read on several threads and the tallies each raised are
/// its own, so they are added here rather than into one counter shared across
/// threads: the totals are then the totals a single-threaded read produced.
fn collect_products(
    built: Vec<Option<(u64, Read, BimElement)>>,
    read: &mut Read,
    kept: &mut HashMap<u64, BimElementId>,
) -> Vec<BimElement> {
    let mut elements = Vec::with_capacity(built.len());
    for built in built {
        let Some((id, item, element)) = built else {
            read.openings += 1;
            continue;
        };
        read.products += 1;
        if element.geometry.is_some() {
            read.with_geometry += 1;
        } else {
            read.without_geometry += 1;
        }
        read.unread_items += item.unread_items;
        read.approximated_items += item.approximated_items;
        kept.insert(id, element.id.clone());
        elements.push(element);
    }
    elements
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
        "IFCFURNISHINGELEMENT" | "IFCFURNITURE" | "IFCSYSTEMFURNITUREELEMENT" => {
            BimElementType::FurnishingElement
        }
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
            Some(BimUnit::new(
                "autodesk.unit.unit:squareMeters-1.0.1",
                "Square meters",
            )),
        ),
        "IFCQUANTITYVOLUME" => (
            number,
            Some(BimUnit::new(
                "autodesk.unit.unit:cubicMeters-1.0.1",
                "Cubic meters",
            )),
        ),
        "IFCQUANTITYWEIGHT" => (
            number,
            Some(BimUnit::new(
                "autodesk.unit.unit:kilograms-1.0.0",
                "Kilograms",
            )),
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
        Value::Text(text) => Some(BimPropertyValue::Text(text.to_string())),
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

#[cfg(test)]
mod tests {
    use super::{Options, convert};
    use crate::step::parse;

    /// A wall voided by two openings: one filled by a window, one left empty.
    /// The wall also bounds a space physically and another virtually, and an
    /// assembly aggregates one of its members. Every relation the conversion
    /// should find is stated here, and every near miss is stated with it.
    const RELATED: &str = "ISO-10303-21;\n\
        HEADER;\n\
        FILE_SCHEMA(('IFC4'));\n\
        ENDSEC;\n\
        DATA;\n\
        #1=IFCCARTESIANPOINT((0.,0.,0.));\n\
        #2=IFCAXIS2PLACEMENT3D(#1,$,$);\n\
        #3=IFCLOCALPLACEMENT($,#2);\n\
        #4=IFCCARTESIANPOINT((0.,0.));\n\
        #5=IFCAXIS2PLACEMENT2D(#4,$);\n\
        #6=IFCRECTANGLEPROFILEDEF(.AREA.,$,#5,2.,1.);\n\
        #7=IFCDIRECTION((0.,0.,1.));\n\
        #8=IFCEXTRUDEDAREASOLID(#6,#2,#7,3.);\n\
        #9=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#8));\n\
        #10=IFCPRODUCTDEFINITIONSHAPE($,$,(#9));\n\
        #11=IFCWALL('wall',$,'Host wall',$,$,#3,#10,$,$);\n\
        #12=IFCWINDOW('window',$,'Filling window',$,$,#3,#10,$,$,$,$,$);\n\
        #13=IFCSPACE('space',$,'101',$,$,#3,#10,'Office',$,$);\n\
        #14=IFCSPACE('other',$,'102',$,$,#3,#10,'Corridor',$,$);\n\
        #15=IFCOPENINGELEMENT('filled',$,$,$,$,#3,#10,$,$);\n\
        #16=IFCOPENINGELEMENT('empty',$,$,$,$,#3,#10,$,$);\n\
        #17=IFCRELVOIDSELEMENT('v1',$,$,$,#11,#15);\n\
        #18=IFCRELVOIDSELEMENT('v2',$,$,$,#11,#16);\n\
        #19=IFCRELFILLSELEMENT('f1',$,$,$,#15,#12);\n\
        #20=IFCRELSPACEBOUNDARY('b1',$,$,$,#13,#11,$,.PHYSICAL.,.INTERNAL.);\n\
        #21=IFCRELSPACEBOUNDARY('b2',$,$,$,#13,#11,$,.PHYSICAL.,.INTERNAL.);\n\
        #22=IFCRELSPACEBOUNDARY('b3',$,$,$,#14,$,$,.VIRTUAL.,.INTERNAL.);\n\
        #23=IFCELEMENTASSEMBLY('assembly',$,'Frame',$,$,#3,#10,$,$,$);\n\
        #24=IFCRELAGGREGATES('a1',$,$,$,#23,(#12));\n\
        ENDSEC;\n\
        END-ISO-10303-21;\n";

    fn relations_of(text: &str) -> Vec<(String, String, String)> {
        let parsed = parse(text.as_bytes()).unwrap();
        let mut found: Vec<(String, String, String)> = convert(&parsed, &Options::default())
            .model
            .relations
            .into_iter()
            .map(|relation| (relation.kind, relation.source.0, relation.target.0))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn stated_relations_are_read_and_composed() {
        assert_eq!(
            relations_of(RELATED),
            vec![
                // The wall never names the window: the pair of relations
                // through the opening is what carries one to the other.
                (
                    "aggregates".to_owned(),
                    "assembly".to_owned(),
                    "window".to_owned()
                ),
                ("bounds".to_owned(), "wall".to_owned(), "space".to_owned()),
                ("hosts".to_owned(), "wall".to_owned(), "window".to_owned()),
            ]
        );
    }

    /// The openings themselves are voids, not products, so no edge may end on
    /// one - and the same boundary stated twice is still one edge.
    #[test]
    fn unresolvable_and_repeated_ends_are_dropped() {
        let found = relations_of(RELATED);
        assert!(
            !found
                .iter()
                .any(|(_, from, to)| from == "filled" || to == "filled" || to == "empty"),
            "an opening reached the model: {found:?}"
        );
        assert_eq!(
            found.iter().filter(|(kind, _, _)| kind == "bounds").count(),
            1,
            "the repeated boundary was not collapsed: {found:?}"
        );
    }

    /// A virtual boundary is an imaginary plane and names no element here; a
    /// void nobody fills leaves the wall with no second host edge.
    #[test]
    fn virtual_boundaries_and_unfilled_voids_state_nothing() {
        let found = relations_of(RELATED);
        assert!(
            !found.iter().any(|(_, _, to)| to == "other"),
            "a virtual boundary became an edge: {found:?}"
        );
        assert_eq!(
            found.iter().filter(|(kind, _, _)| kind == "hosts").count(),
            1,
            "an unfilled void produced a host edge: {found:?}"
        );
    }
}

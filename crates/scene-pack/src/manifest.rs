//! The manifest: everything about a scene that is not a triangle.
//!
//! It is JSON because a viewer reads it once and then works from the binary
//! sections it points at. It is kept in parallel arrays and a string table
//! rather than one object per element, because an object per element is where
//! a text format goes wrong at model scale: the same category name written
//! 40 000 times is 40 000 copies in JSON and one entry here.

use std::collections::HashMap;

use bim_core::{
    BimElement, BimElementType, BimMaterialLayerSet, BimModel, BimProperty, BimPropertyValue,
};
use serde_json::{Value, json};

use bim_convert::{ifc_entity_name, resolved_element_type};

use crate::chunk::ChunkBuilder;
use crate::{SourceInfo, Stats, VERSION};

/// Where one section sits in the file, and how big it is once inflated.
#[derive(Clone, Copy, Debug, Default)]
pub struct Span {
    pub offset: u64,
    /// Bytes on disk, raw-deflate.
    pub stored: u64,
    /// Bytes once inflated.
    pub length: u64,
}

impl Span {
    fn to_json(self) -> Value {
        json!({ "offset": self.offset, "stored": self.stored, "length": self.length })
    }
}

/// The manifest under construction.
pub struct Builder<'model> {
    model: &'model BimModel,
    source: SourceInfo,
    strings: Vec<String>,
    interned: HashMap<String, u32>,
    classes: Table,
    categories: Table,
    kinds: Table,
    ifc_classes: Table,
    levels: Vec<(String, Option<String>, Option<f64>)>,
    level_index: HashMap<String, usize>,
    has_geometry: Vec<bool>,
    chunks: Vec<Value>,
    property_blocks: Vec<Span>,
    property_block_size: usize,
    element_bounds: Span,
}

/// A set of names with a count each, in first-seen order.
#[derive(Default)]
struct Table {
    names: Vec<String>,
    counts: Vec<u32>,
    index: HashMap<String, usize>,
}

impl Table {
    fn insert(&mut self, name: Option<&str>) -> i32 {
        let Some(name) = name else {
            return -1;
        };
        let at = *self.index.entry(name.to_owned()).or_insert_with(|| {
            self.names.push(name.to_owned());
            self.counts.push(0);
            self.names.len() - 1
        });
        self.counts[at] += 1;
        i32::try_from(at).unwrap_or(-1)
    }

    fn to_json(&self) -> Value {
        Value::Array(
            self.names
                .iter()
                .zip(&self.counts)
                .map(|(name, count)| json!({ "name": name, "count": count }))
                .collect(),
        )
    }
}

impl<'model> Builder<'model> {
    pub fn new(model: &'model BimModel, source: &SourceInfo) -> Self {
        let mut levels = Vec::with_capacity(model.levels.len());
        let mut level_index = HashMap::with_capacity(model.levels.len());
        for level in &model.levels {
            level_index.insert(level.id.0.clone(), levels.len());
            levels.push((
                level.id.0.clone(),
                level.name.clone(),
                level.elevation.as_ref().map(|number| number.value),
            ));
        }
        Self {
            model,
            source: source.clone(),
            strings: Vec::new(),
            interned: HashMap::new(),
            classes: Table::default(),
            categories: Table::default(),
            kinds: Table::default(),
            ifc_classes: Table::default(),
            levels,
            level_index,
            has_geometry: vec![false; model.elements.len()],
            chunks: Vec::new(),
            property_blocks: Vec::new(),
            property_block_size: 1,
            element_bounds: Span::default(),
        }
    }

    pub fn mark_geometry(&mut self, element: usize) {
        if let Some(slot) = self.has_geometry.get_mut(element) {
            *slot = true;
        }
    }

    pub fn set_element_bounds(&mut self, span: Span) {
        self.element_bounds = span;
    }

    pub fn push_property_block(&mut self, span: Span) {
        self.property_blocks.push(span);
    }

    pub fn set_property_block_size(&mut self, size: usize) {
        self.property_block_size = size.max(1);
    }

    pub fn push_chunk(&mut self, span: Span, chunk: &ChunkBuilder) {
        let (min, max) = chunk.bounds();
        self.chunks.push(json!({
            "offset": span.offset,
            "stored": span.stored,
            "length": span.length,
            "min": min,
            "max": max,
            "vertices": chunk.vertex_count(),
            "triangles": chunk.triangle_count(),
            "edges": chunk.edge_count(),
            "elements": chunk.element_count(),
        }));
    }

    fn intern(&mut self, value: Option<&str>) -> i32 {
        let Some(value) = value else {
            return -1;
        };
        let at = *self.interned.entry(value.to_owned()).or_insert_with(|| {
            self.strings.push(value.to_owned());
            u32::try_from(self.strings.len() - 1).unwrap_or(u32::MAX)
        });
        i32::try_from(at).unwrap_or(-1)
    }

    #[allow(clippy::too_many_lines)] // One pass that builds every parallel column.
    pub fn finish(mut self, stats: &Stats) -> Value {
        let count = self.model.elements.len();
        let mut ids = Vec::with_capacity(count);
        let mut names = Vec::with_capacity(count);
        let mut long_names = Vec::with_capacity(count);
        let mut classes = Vec::with_capacity(count);
        let mut categories = Vec::with_capacity(count);
        let mut kinds = Vec::with_capacity(count);
        let mut ifc_classes = Vec::with_capacity(count);
        let mut levels = Vec::with_capacity(count);
        let mut types = Vec::with_capacity(count);
        let mut geometry = Vec::with_capacity(count);
        let mut documents = Vec::with_capacity(count);
        let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(count);
        // Where a document each element names, so that a federated scene can
        // be filtered by source file and each element's source class read
        // against the format that actually stated it.
        let document_index: HashMap<&str, (usize, &str)> = self
            .model
            .documents
            .iter()
            .enumerate()
            .map(|(at, document)| (document.id.0.as_str(), (at, document.kind.as_str())))
            .collect();

        for (index, element) in self.model.elements.iter().enumerate() {
            by_id.insert(element.id.0.as_str(), index);
            ids.push(element.id.0.clone());
            let name = self.intern(element.name.as_deref());
            names.push(name);
            let long_name = self.intern(element.long_name.as_deref());
            long_names.push(long_name);
            classes.push(self.classes.insert(element.class_name.as_deref()));
            categories.push(
                self.categories.insert(
                    element
                        .category
                        .as_ref()
                        .map(|category| category.name.as_str()),
                ),
            );
            kinds.push(self.kinds.insert(Some(kind_name(element.element_type))));
            // For an imported IFC, the source entity is the exact class the
            // viewer must show. Mapping it back through `BimElementType` would
            // collapse source classes without a canonical counterpart (beams,
            // reinforcing bars, footings, ...) into a building-element proxy.
            // An RVT has no source IFC class, so there the export mapping is
            // still the useful answer: the entity `export-ifc` would write.
            let document = element
                .document
                .as_ref()
                .and_then(|id| document_index.get(id.0.as_str()).copied());
            documents.push(
                document
                    .and_then(|(at, _)| i32::try_from(at).ok())
                    .unwrap_or(-1),
            );
            // The format that stated *this* element, which in a federated set
            // of mixed sources is not the set's own kind.
            let from_ifc = document.map_or_else(
                || self.source.kind.eq_ignore_ascii_case("ifc"),
                |(_, kind)| kind.eq_ignore_ascii_case("ifc"),
            );
            let ifc_class = if from_ifc {
                element
                    .class_name
                    .as_deref()
                    .filter(|name| name.starts_with("IFC"))
                    .unwrap_or_else(|| ifc_entity_name(resolved_element_type(element)))
            } else {
                ifc_entity_name(resolved_element_type(element))
            };
            ifc_classes.push(self.ifc_classes.insert(Some(ifc_class)));
            levels.push(
                element
                    .level_id
                    .as_ref()
                    .and_then(|id| self.level_index.get(id.0.as_str()))
                    .and_then(|at| i32::try_from(*at).ok())
                    .unwrap_or(-1),
            );
            // A type is shown by name where it has one and only falls back to
            // its identifier, which is a GUID or a record number.
            types.push(
                self.intern(
                    element
                        .type_name
                        .as_deref()
                        .or_else(|| element.type_id.as_ref().map(|id| id.0.as_str())),
                ),
            );
            geometry.push(u8::from(self.has_geometry[index]));
        }
        let documents_json: Vec<Value> = self
            .model
            .documents
            .iter()
            .map(|document| {
                json!({
                    "id": document.id.0,
                    "name": document.name,
                    "kind": document.kind,
                    "application": document.source.as_ref().map(|source| &source.application),
                    "release": document.source.as_ref().and_then(|source| source.release.as_ref()),
                    "elements": document.elements,
                })
            })
            .collect();

        let relations: Vec<Value> = self
            .model
            .relations
            .iter()
            .filter_map(|relation| {
                let from = *by_id.get(relation.source.0.as_str())?;
                let to = *by_id.get(relation.target.0.as_str())?;
                Some(json!({ "kind": relation.kind, "from": from, "to": to }))
            })
            .collect();

        let levels_json: Vec<Value> = self
            .levels
            .iter()
            .map(|(id, name, elevation)| json!({ "id": id, "name": name, "elevation": elevation }))
            .collect();

        json!({
            "format": "rivet-scene",
            "version": VERSION,
            "unit": "metre",
            "source": {
                "name": self.source.name,
                "kind": self.source.kind,
                "application": self.source.application,
                "release": self.source.release,
            },
            "counts": {
                "elements": stats.elements,
                "withGeometry": stats.elements_with_geometry,
                "vertices": stats.vertices,
                "triangles": stats.triangles,
                "edges": stats.edges,
                "skippedFaces": stats.skipped_faces,
            },
            "strings": self.strings,
            "classes": self.classes.to_json(),
            "categories": self.categories.to_json(),
            "kinds": self.kinds.to_json(),
            "ifcClasses": self.ifc_classes.to_json(),
            "levels": levels_json,
            // One entry per source file. A scene read from a single file has
            // one; a federated scene has one per file, and every element's
            // `documents` entry indexes into this list.
            "documents": documents_json,
            "elements": {
                "ids": ids,
                "names": names,
                "longNames": long_names,
                "classes": classes,
                "categories": categories,
                "kinds": kinds,
                "ifcClasses": ifc_classes,
                "levels": levels,
                "types": types,
                "geometry": geometry,
                "documents": documents,
            },
            "elementBounds": self.element_bounds.to_json(),
            "chunks": self.chunks,
            "properties": {
                "blockSize": self.property_block_size,
                "blocks": self.property_blocks
                    .iter()
                    .map(|span| span.to_json())
                    .collect::<Vec<Value>>(),
            },
            "relations": relations,
        })
    }
}

/// One block of elements' properties, as the JSON a viewer shows in its panel.
pub fn property_block(elements: &[BimElement]) -> String {
    let entries: Vec<Value> = elements
        .iter()
        .map(|element| {
            let mut properties: Vec<Value> = element
                .properties
                .iter()
                .map(|property| property_json(property, false))
                .collect();
            properties.extend(
                element
                    .type_properties
                    .iter()
                    .map(|property| property_json(property, true)),
            );
            let mut entry = json!({ "properties": properties });
            if let Some(layers) = element.material_layers.as_ref() {
                entry["layers"] = layer_set_json(layers);
            }
            if let Some(placement) = element.placement.as_ref() {
                entry["placement"] = json!({
                    "origin": placement.origin.coordinates,
                    "referenceDirection": placement.reference_direction,
                    "axis": placement.axis,
                });
            }
            entry
        })
        .collect();
    Value::Array(entries).to_string()
}

fn property_json(property: &BimProperty, from_type: bool) -> Value {
    let mut entry = json!({ "name": property.name, "value": value_json(&property.value) });
    if let BimPropertyValue::Number(number) = &property.value {
        if let Some(unit) = number.unit.as_ref() {
            entry["unit"] = json!(unit.name);
            entry["unitId"] = json!(unit.id);
        }
    }
    if let Some(specification) = property.specification.as_ref() {
        entry["spec"] = json!(specification);
    }
    if let Some(id) = property.id.as_ref() {
        entry["id"] = json!(id.value);
        entry["idSystem"] = json!(id.system);
    }
    if from_type {
        entry["from"] = json!("type");
    }
    entry
}

/// A property's value, with anything undecoded reported as its size rather
/// than as a number the source never stated.
fn value_json(value: &BimPropertyValue) -> Value {
    match value {
        BimPropertyValue::Bool(flag) => json!(flag),
        BimPropertyValue::Integer(number) => json!(number),
        BimPropertyValue::Number(number) => json!(number.value),
        BimPropertyValue::Text(text) => json!(text),
        BimPropertyValue::Reference(id) => json!({ "reference": id.0 }),
        BimPropertyValue::Bytes(bytes) => json!({ "bytes": bytes.len() }),
        BimPropertyValue::Unknown(bytes) => json!({ "undecoded": bytes.len() }),
    }
}

fn layer_set_json(layers: &BimMaterialLayerSet) -> Value {
    json!({
        "name": layers.name,
        "sourceTypeId": layers.source_type_id.as_ref().map(|id| id.0.clone()),
        "totalThickness": layers.total_thickness().map(|number| number.value),
        "layers": layers.layers.iter().map(|layer| json!({
            "material": layer.material.as_ref().and_then(|material| material.name.clone()),
            "thickness": layer.thickness.value,
            "unit": layer.thickness.unit.as_ref().map(|unit| unit.name.clone()),
            "core": layer.is_core,
            "structural": layer.is_structural,
            "sourceFunction": layer.source_function,
        })).collect::<Vec<Value>>(),
    })
}

/// The name a semantic type is published under. These are `bim-core`'s own
/// names, not IFC entity names: the mapping to IFC is the exporter's business
/// and is not repeated here.
fn kind_name(kind: BimElementType) -> &'static str {
    match kind {
        BimElementType::Unknown => "Unknown",
        BimElementType::PipeSegment => "PipeSegment",
        BimElementType::PipeFitting => "PipeFitting",
        BimElementType::SanitaryTerminal => "SanitaryTerminal",
        BimElementType::AirTerminal => "AirTerminal",
        BimElementType::FireSuppressionTerminal => "FireSuppressionTerminal",
        BimElementType::Alarm => "Alarm",
        BimElementType::CableCarrierFitting => "CableCarrierFitting",
        BimElementType::DuctSegment => "DuctSegment",
        BimElementType::CableCarrierSegment => "CableCarrierSegment",
        BimElementType::DistributionElement => "DistributionElement",
        BimElementType::DistributionFlowElement => "DistributionFlowElement",
        BimElementType::Wall => "Wall",
        BimElementType::Slab => "Slab",
        BimElementType::Roof => "Roof",
        BimElementType::Stair => "Stair",
        BimElementType::StairFlight => "StairFlight",
        BimElementType::Railing => "Railing",
        BimElementType::FurnishingElement => "FurnishingElement",
        BimElementType::Column => "Column",
        BimElementType::Member => "Member",
        BimElementType::Plate => "Plate",
        BimElementType::Window => "Window",
        BimElementType::Door => "Door",
        BimElementType::CurtainWall => "CurtainWall",
        BimElementType::Space => "Space",
    }
}

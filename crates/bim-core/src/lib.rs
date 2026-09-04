#![forbid(unsafe_code)]

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BimModel {
    /// Application/release that supplied the data, retained for provenance.
    pub source: Option<BimSource>,
    pub elements: Vec<BimElement>,
    pub levels: Vec<BimLevel>,
    pub relations: Vec<BimRelation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimSource {
    pub application: String,
    pub release: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimElement {
    pub id: BimElementId,
    /// Format-neutral semantic type. `Unknown` preserves elements that have
    /// not yet been classified without forcing an exporter-specific guess.
    pub element_type: BimElementType,
    /// Format-neutral source class, such as `Wall` or `FamilyInstance`.
    pub class_name: Option<String>,
    pub name: Option<String>,
    pub category: Option<BimCategory>,
    pub level_id: Option<BimElementId>,
    pub type_id: Option<BimElementId>,
    pub placement: Option<BimPlacement>,
    pub geometry: Option<BimGeometry>,
    pub properties: Vec<BimProperty>,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum BimElementType {
    #[default]
    Unknown,
    PipeSegment,
    PipeFitting,
    SanitaryTerminal,
    AirTerminal,
    FireSuppressionTerminal,
    Alarm,
    CableCarrierFitting,
    /// Source category that is certainly an MEP distribution element without
    /// naming the device. Recorded as the supertype rather than guessed.
    DistributionElement,
    /// Source category that is certainly a flow element (a valve, strainer,
    /// meter, pump or air handler) without naming which one.
    DistributionFlowElement,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimGeometry {
    /// An exact source centerline without an inferred body/profile.
    AxisLine(BimLineSegment),
    SweptDisk(BimSweptDisk),
    /// An independently verified element-local extent. This is a bounding
    /// representation, not a claim about the element's body or topology.
    BoundingBox(BimBoundingBox),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimBoundingBox {
    pub min: BimPoint3,
    pub max: BimPoint3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimSweptDisk {
    pub directrix: BimLineSegment,
    pub radius: BimNumber,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimLineSegment {
    pub start: BimPoint3,
    pub end: BimPoint3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimPoint3 {
    pub coordinates: [f64; 3],
    pub unit: BimUnit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimPlacement {
    /// Placement origin in source/world coordinates.
    pub origin: BimPoint3,
    /// Local X direction expressed in the same world coordinate system.
    pub reference_direction: [f64; 3],
    /// Local Z direction expressed in the same world coordinate system.
    pub axis: [f64; 3],
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BimElementId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimCategory {
    pub id: Option<BimExternalId>,
    pub name: String,
}

/// Identifier in a source-system namespace. Keeping the namespace prevents a
/// Revit enum value from being confused with an IFC classification code.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimExternalId {
    pub system: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimProperty {
    pub id: Option<BimExternalId>,
    pub name: String,
    /// Forge spec or another source-independent quantity-kind identifier.
    pub specification: Option<String>,
    pub value: BimPropertyValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BimPropertyValue {
    Bool(bool),
    Integer(i64),
    Number(BimNumber),
    Text(String),
    Reference(BimElementId),
    Bytes(Vec<u8>),
    Unknown(Vec<u8>),
}

/// A number paired with the unit in which `value` is expressed. `None` means
/// the dimension has not been established; it does not mean unitless.
#[derive(Clone, Debug, PartialEq)]
pub struct BimNumber {
    pub value: f64,
    pub unit: Option<BimUnit>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BimUnit {
    /// Stable external identifier, for example a Forge unit type ID.
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BimRelation {
    pub kind: String,
    pub source: BimElementId,
    pub target: BimElementId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BimLevel {
    pub id: BimElementId,
    pub name: Option<String>,
    /// Elevation in a canonical, explicitly named unit.
    pub elevation: Option<BimNumber>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_unit_is_distinct_from_a_unitless_quantity() {
        let unknown = BimNumber {
            value: 12.0,
            unit: None,
        };
        let unitless = BimNumber {
            value: 12.0,
            unit: Some(BimUnit {
                id: "autodesk.unit.unit:general-1.0.1".to_owned(),
                name: "General".to_owned(),
            }),
        };
        assert_ne!(unknown, unitless);
    }
}

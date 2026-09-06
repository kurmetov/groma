use bim_core::{BimElement, BimElementType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceMapping {
    class_name: Option<&'static str>,
    category_name: &'static str,
    element_type: BimElementType,
}

/// Source classes that determine the element type on their own, without a
/// category. These are Revit's architectural system families, and the mapping
/// is not inferred: joining our decode to the IFC Revit itself exported from
/// the same model on the Revit element id, each of these classes maps to one
/// IFC entity with no spread at all - `SWall` is `IfcWall` for 7 610 of 7 610
/// matched elements, `Floor` is `IfcSlab` for 527 of 527, and the stair and
/// roof classes for every one of theirs. A loadable `FamilyInstance` is
/// deliberately absent: it spreads across eight IFC entities in the same
/// join, so its category is required and the class alone may not stand in.
const CLASS_MAPPINGS: &[(&str, BimElementType)] = &[
    ("SWall", BimElementType::Wall),
    ("Floor", BimElementType::Slab),
    // A stair landing is a slab in IFC, which is what Revit emits for it.
    ("StairsLanding", BimElementType::Slab),
    ("StairsRun", BimElementType::StairFlight),
    ("StairsElement", BimElementType::Stair),
    ("ProfileRoof", BimElementType::Roof),
    // A room is not a building element, but it is established by its class in
    // the same way and against the same reference: Revit's export of AR S1
    // carries 553 `IfcSpace` and the decode yields 554 `RoomElem`, each with a
    // level, a room name and - for 553 of them - a number.
    ("RoomElem", BimElementType::Space),
];

/// Ordered, conservative source mapping. `class_name: None` means that the
/// category is sufficient; a named class must match together with category.
const SOURCE_MAPPINGS: &[SourceMapping] = &[
    SourceMapping {
        class_name: Some("RbsPipeCurve"),
        category_name: "OST_PipeCurves",
        element_type: BimElementType::PipeSegment,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_PipeFitting",
        element_type: BimElementType::PipeFitting,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_PlumbingFixtures",
        element_type: BimElementType::SanitaryTerminal,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_DuctTerminal",
        element_type: BimElementType::AirTerminal,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_Sprinklers",
        element_type: BimElementType::FireSuppressionTerminal,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_FireAlarmDevices",
        element_type: BimElementType::Alarm,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_CableTrayFitting",
        element_type: BimElementType::CableCarrierFitting,
    },
    // The category names below identify the discipline but not the device, so
    // they resolve to the IFC supertype instead of guessing a leaf entity.
    SourceMapping {
        class_name: None,
        category_name: "OST_ElectricalEquipment",
        element_type: BimElementType::DistributionElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_PipeAccessory",
        element_type: BimElementType::DistributionFlowElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_DuctAccessory",
        element_type: BimElementType::DistributionFlowElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_MechanicalEquipment",
        element_type: BimElementType::DistributionFlowElement,
    },
];

/// Infer a format-neutral element type from the source class/category pair.
/// Unknown and incomplete pairs are deliberately left unclassified.
#[must_use]
pub fn element_type_for_source(
    class_name: Option<&str>,
    category_name: Option<&str>,
) -> BimElementType {
    let Some(category_name) = category_name else {
        // No declared category. For these classes that is the signal that the
        // record is an instance rather than a type or definition - in the
        // reference join not one of Revit's 11 518 products declares a
        // category of its own - so the class alone establishes the type.
        return class_name
            .and_then(|class_name| {
                CLASS_MAPPINGS
                    .iter()
                    .find(|(mapped, _)| *mapped == class_name)
                    .map(|(_, element_type)| *element_type)
            })
            .unwrap_or(BimElementType::Unknown);
    };
    SOURCE_MAPPINGS
        .iter()
        .find(|mapping| {
            mapping.category_name == category_name
                && mapping
                    .class_name
                    .is_none_or(|required| class_name == Some(required))
        })
        .map_or(BimElementType::Unknown, |mapping| mapping.element_type)
}

pub(crate) fn resolved_element_type(element: &BimElement) -> BimElementType {
    if element.element_type == BimElementType::Unknown {
        element_type_for_source(
            element.class_name.as_deref(),
            element
                .category
                .as_ref()
                .map(|category| category.name.as_str()),
        )
    } else {
        element.element_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_is_a_space_by_its_class_and_a_categorised_one_is_not() {
        // No declared category: the class establishes the type, as it does for
        // the other architectural classes.
        assert_eq!(
            element_type_for_source(Some("RoomElem"), None),
            BimElementType::Space
        );
        assert!(BimElementType::Space.is_spatial());
        // A record that declares a category is a type or a definition, and
        // nothing in the category table names a space.
        assert_eq!(
            element_type_for_source(Some("RoomElem"), Some("OST_Rooms")),
            BimElementType::Unknown
        );
        // The building elements stay elements.
        assert!(!BimElementType::Wall.is_spatial());
        assert!(!BimElementType::Unknown.is_spatial());
    }

    #[test]
    fn maps_the_supported_source_pairs() {
        for (class_name, category_name, expected) in [
            (
                Some("RbsPipeCurve"),
                "OST_PipeCurves",
                BimElementType::PipeSegment,
            ),
            (
                Some("FamilyInstance"),
                "OST_PipeFitting",
                BimElementType::PipeFitting,
            ),
            (
                Some("FamilyInstance"),
                "OST_PlumbingFixtures",
                BimElementType::SanitaryTerminal,
            ),
            (
                Some("FamilyInstance"),
                "OST_DuctTerminal",
                BimElementType::AirTerminal,
            ),
            (
                Some("FamilyInstance"),
                "OST_Sprinklers",
                BimElementType::FireSuppressionTerminal,
            ),
            (
                Some("FamilyInstance"),
                "OST_FireAlarmDevices",
                BimElementType::Alarm,
            ),
            (
                Some("FamilyInstance"),
                "OST_CableTrayFitting",
                BimElementType::CableCarrierFitting,
            ),
            (
                Some("FamilyInstance"),
                "OST_ElectricalEquipment",
                BimElementType::DistributionElement,
            ),
            (
                Some("FamilyInstance"),
                "OST_PipeAccessory",
                BimElementType::DistributionFlowElement,
            ),
            (
                Some("FamilyInstance"),
                "OST_DuctAccessory",
                BimElementType::DistributionFlowElement,
            ),
            (
                Some("FamilyInstance"),
                "OST_MechanicalEquipment",
                BimElementType::DistributionFlowElement,
            ),
        ] {
            assert_eq!(
                element_type_for_source(class_name, Some(category_name)),
                expected
            );
        }
    }

    #[test]
    fn types_an_architectural_system_family_from_its_class_alone() {
        // Verified against the IFC Revit exported from the same model: joined
        // on the Revit element id, each of these classes maps to exactly one
        // IFC entity, with no spread.
        for (class_name, expected) in [
            ("SWall", BimElementType::Wall),
            ("Floor", BimElementType::Slab),
            ("StairsLanding", BimElementType::Slab),
            ("StairsRun", BimElementType::StairFlight),
            ("StairsElement", BimElementType::Stair),
            ("ProfileRoof", BimElementType::Roof),
        ] {
            assert_eq!(element_type_for_source(Some(class_name), None), expected);
        }
    }

    #[test]
    fn refuses_the_class_shortcut_for_a_record_that_declares_a_category() {
        // A record of one of those classes that declares its own category is a
        // type or definition, not an instance - no product Revit exports
        // declares one - so it must not be typed as the product.
        assert_eq!(
            element_type_for_source(Some("SWall"), Some("OST_Walls")),
            BimElementType::Unknown
        );
        assert_eq!(
            element_type_for_source(Some("Floor"), Some("OST_Floors")),
            BimElementType::Unknown
        );
    }

    #[test]
    fn refuses_to_type_a_family_instance_from_its_class() {
        // `FamilyInstance` spreads across eight IFC entities in the reference
        // join - railing, column, window, door, member, plate, proxy and
        // opening - so the class alone may not stand in for its category.
        assert_eq!(
            element_type_for_source(Some("FamilyInstance"), None),
            BimElementType::Unknown
        );
    }

    #[test]
    fn requires_both_pipe_curve_discriminators() {
        assert_eq!(
            element_type_for_source(Some("FamilyInstance"), Some("OST_PipeCurves")),
            BimElementType::Unknown
        );
        assert_eq!(
            element_type_for_source(Some("RbsPipeCurve"), Some("OST_DuctCurves")),
            BimElementType::Unknown
        );
    }

    #[test]
    fn leaves_unknown_or_incomplete_sources_unclassified() {
        assert_eq!(
            element_type_for_source(Some("Wall"), Some("OST_Walls")),
            BimElementType::Unknown
        );
        // A generic model carries no discipline, and a model group is not a
        // product at all; neither may be typed from the category alone.
        for category_name in ["OST_GenericModel", "OST_IOSModelGroups"] {
            assert_eq!(
                element_type_for_source(Some("FamilyInstance"), Some(category_name)),
                BimElementType::Unknown
            );
        }
        assert_eq!(
            element_type_for_source(Some("RbsPipeCurve"), None),
            BimElementType::Unknown
        );
    }
}

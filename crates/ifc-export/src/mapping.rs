use bim_core::{BimElement, BimElementType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceMapping {
    class_name: Option<&'static str>,
    category_name: &'static str,
    element_type: BimElementType,
}

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
        return BimElementType::Unknown;
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

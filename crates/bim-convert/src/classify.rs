//! What an element is, read from the vocabulary its source names it with,
//! and what each answer is called in IFC.
//!
//! Both tables live here rather than in a writer because three crates ask
//! the same questions - the RVT reader while it builds a model, the IFC
//! writer while it emits one, and the viewer scene while it labels one - and
//! a second copy of either table would drift from the first.

use bim_core::{BimElement, BimElementType};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceMapping {
    class_name: Option<&'static str>,
    category_name: &'static str,
    element_type: BimElementType,
}

/// Source classes that determine the element type on their own, without a
/// category.
///
/// A loadable `FamilyInstance` is deliberately absent: it spreads across
/// eight IFC entities in the reference join below, so its category is
/// required and the class alone may not stand in. What is here is the system
/// families, whose class is the element type on its own.
///
/// The architectural ones are measured. Joining our decode to the IFC Revit
/// itself exported from the same model on the Revit element id, each maps to
/// one IFC entity with no spread at all - `SWall` is `IfcWall` for 7 610 of
/// 7 610 matched elements, `Floor` is `IfcSlab` for 527 of 527, and the stair
/// and roof classes for every one of theirs.
///
/// The MEP runs below them have no reference export to join against - the
/// corpus has none for a plumbing, ventilation or electrical model - and they
/// are here for a different reason: each already pairs with one element type
/// in [`SOURCE_MAPPINGS`], and a run of that class states no category of its
/// own often enough that requiring one loses most of a model. Measured on a
/// ventilation model of 22 783 elements, 5 432 `RbsPipeCurve` and 750
/// `RbsDuctCurve` records declare no category; on an electrical model of
/// 12 627, 1 355 `RbsConduitCurve` records do not. Without these rows every
/// one of them is an anonymous proxy in a file that names the pipe beside it.
/// A class here says no more than the row it mirrors: an `RbsConduitCurve` is
/// a conduit run whether or not its own record repeats the category.
const CLASS_MAPPINGS: &[(&str, BimElementType)] = &[
    ("RbsPipeCurve", BimElementType::PipeSegment),
    ("RbsFlexPipeCurve", BimElementType::PipeSegment),
    ("RbsDuctCurve", BimElementType::DuctSegment),
    ("RbsFlexDuctCurve", BimElementType::DuctSegment),
    ("RbsConduitCurve", BimElementType::CableCarrierSegment),
    ("CableTray", BimElementType::CableCarrierSegment),
    // The insulation that follows a run, which declares a category no more
    // often than the run does: on one ventilation model 6 331 of its 6 331
    // `RbsPipeInsulation` records state none, and 180 of 1 430
    // `RbsDuctInsulation` records.
    ("RbsPipeInsulation", BimElementType::Insulation),
    ("RbsDuctInsulation", BimElementType::Insulation),
    ("SWall", BimElementType::Wall),
    ("Floor", BimElementType::Slab),
    // A stair landing is a slab in IFC, which is what Revit emits for it.
    ("StairsLanding", BimElementType::Slab),
    ("StairsRun", BimElementType::StairFlight),
    ("StairsElement", BimElementType::Stair),
    ("ProfileRoof", BimElementType::Roof),
    // A railing drawn as a system family rather than placed as a loadable
    // one. `OST_StairsRailing` below reaches the second kind through its
    // family; this reaches the first, which declares no category at all, and
    // which nothing else admitted - on a second reference export Revit writes
    // `IfcRailing` for seven records of this class and we wrote nothing for
    // any of them.
    //
    // It costs precision to buy that recall, and the cost is stated rather
    // than left to be found: the same file has fourteen records of the class,
    // Revit exports seven, and no field this decode recovers separates the
    // two halves - none of the fourteen carries a level, a type, a host or a
    // name. That is the trade the classes above it were accepted on as well,
    // at 56.9% to 69.3% precision for 100% recall. Every one of the fourteen
    // is bodiless, so the default export writes none of them and the cost is
    // paid only by a scene and by `--elements-without-a-body`.
    ("BaseRailing", BimElementType::Railing),
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
    // The other runs of a building system, by the category their own type
    // declares. `OST_PipeCurves` above is the same reading one class narrower;
    // these are what the electrical and ventilation models are made of, and
    // the corpus has no reference export for either, so they rest on the
    // category alone - see `BimElementType::DuctSegment`.
    SourceMapping {
        class_name: Some("RbsFlexPipeCurve"),
        category_name: "OST_FlexPipeCurves",
        element_type: BimElementType::PipeSegment,
    },
    SourceMapping {
        class_name: Some("RbsDuctCurve"),
        category_name: "OST_DuctCurves",
        element_type: BimElementType::DuctSegment,
    },
    SourceMapping {
        class_name: Some("RbsFlexDuctCurve"),
        category_name: "OST_FlexDuctCurves",
        element_type: BimElementType::DuctSegment,
    },
    SourceMapping {
        class_name: Some("RbsConduitCurve"),
        category_name: "OST_Conduit",
        element_type: BimElementType::CableCarrierSegment,
    },
    SourceMapping {
        class_name: Some("CableTray"),
        category_name: "OST_CableTray",
        element_type: BimElementType::CableCarrierSegment,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_PipeFitting",
        element_type: BimElementType::PipeFitting,
    },
    // The duct run's fitting, beside the pipe run's above it. The two
    // categories are the same statement about two systems and IFC has an
    // entity for each, so the row that carried only one of them left every
    // bend of a ventilation model an anonymous proxy - 792 on one model of
    // 22 783.
    SourceMapping {
        class_name: None,
        category_name: "OST_DuctFitting",
        element_type: BimElementType::DuctFitting,
    },
    // The insulation wrapping a run. IFC has no entity for it and does not
    // need one: a covering is the thing that clads another, and `INSULATION`
    // is one of the kinds `IfcCovering` declares - which is how Revit's own
    // exporter writes it too. No reference export measures the pairing here,
    // the corpus having none for a mechanical model; what stands behind it is
    // that the schema names this kind and the source names this category, and
    // the two say the same thing.
    SourceMapping {
        class_name: None,
        category_name: "OST_PipeInsulations",
        element_type: BimElementType::Insulation,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_DuctInsulations",
        element_type: BimElementType::Insulation,
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
    // A conduit fitting is the cable tray fitting's sibling: both are the
    // fitting of a cable carrier, and IFC has the one entity for them. The
    // row above carried only one of the two, so on an electrical model every
    // conduit bend arrived as a proxy - 1 787 of them over three.
    SourceMapping {
        class_name: None,
        category_name: "OST_ConduitFitting",
        element_type: BimElementType::CableCarrierFitting,
    },
    // The one electrical category IFC names outright. `IfcLightFixture` is
    // defined as a lighting fixture and nothing else, so the pairing is the
    // name rather than a reading of it.
    SourceMapping {
        class_name: None,
        category_name: "OST_LightingFixtures",
        element_type: BimElementType::LightFixture,
    },
    // The category names below identify the discipline but not the device, so
    // they resolve to the IFC supertype instead of guessing a leaf entity.
    SourceMapping {
        class_name: None,
        category_name: "OST_ElectricalEquipment",
        element_type: BimElementType::DistributionElement,
    },
    // The two categories an electrical model is mostly made of. Both are
    // certainly distribution elements and neither names the device: a
    // lighting device is a switch, a dimmer or a sensor, and an electrical
    // fixture is a socket, a junction box or a floor box. IFC has a leaf for
    // several of those - `IfcSwitchingDevice`, `IfcOutlet` - and no way to
    // tell from the category which one a given instance is, so the supertype
    // is recorded rather than a leaf guessed. Together they are 13 988 of the
    // 22 169 elements of three electrical models, so what this decides is
    // most of what such a model holds.
    SourceMapping {
        class_name: None,
        category_name: "OST_LightingDevices",
        element_type: BimElementType::DistributionElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_ElectricalFixtures",
        element_type: BimElementType::DistributionElement,
    },
    // Revit's other device categories, which say the same thing about the
    // low-voltage systems that the two above say about power and lighting.
    // They are here as a set rather than one at a time as a model turns one
    // up: the statement each of them makes is the same, and a data device
    // left as a proxy while a lighting device beside it is typed would be an
    // accident of which model was looked at first. `OST_FireAlarmDevices` is
    // not among them - IFC names that one, and it is an `IfcAlarm` above.
    SourceMapping {
        class_name: None,
        category_name: "OST_DataDevices",
        element_type: BimElementType::DistributionElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_CommunicationDevices",
        element_type: BimElementType::DistributionElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_SecurityDevices",
        element_type: BimElementType::DistributionElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_NurseCallDevices",
        element_type: BimElementType::DistributionElement,
    },
    SourceMapping {
        class_name: None,
        category_name: "OST_TelephoneDevices",
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
    // A loadable family's category, reached through its family. Every row here
    // is measured against Revit's own export of AR S1, joined on the Revit
    // element id, and every one of them has no spread: `OST_StairsRailing` is
    // `IfcRailing` for 881 of 881 elements, `OST_Columns` `IfcColumn` for 192
    // of 192, `OST_StructuralColumns` for 56 of 56, `OST_CurtainWallMullions`
    // `IfcMember` for 134 of 134, `OST_CurtainWallPanels` `IfcPlate` for 37 of
    // 37, `OST_Windows` `IfcWindow` for 474 of 474 and `OST_Doors` `IfcDoor`
    // for 295 of 295. The class is required as well, because the category is
    // only this decisive for an instance of a loadable family.
    //
    // Two categories the same join leaves alone: `OST_StructuralFraming`,
    // which Revit exports as an `IfcBuildingElementProxy` for all 444 of them,
    // and `OST_GenericModel`, which it turns into an `IfcOpeningElement` - a
    // void, which cannot be written without the element it voids.
    //
    // That reading of `OST_GenericModel` is an architectural model's, and it
    // does not travel. Over 40 electrical models of 229 936 elements the
    // category is nearly the whole of what is left untyped - 12 203 of the
    // 12 331, all of them in one project's seven files - and it is not a void
    // there: 3 866 of one model's 3 867 carry a body, and they are named
    // `коробка_потолочная`, `Выключатель одноклавишный`, `гофрированная_труба`.
    // Junction boxes, switches and conduit, drawn in a generic family.
    //
    // No row is added for it on that evidence alone. The category really is
    // where an architectural model puts its openings, so typing it outright
    // would write a void as a product on every AR file this converts. What
    // would separate the two is that an opening names the element it cuts and
    // a switch names nothing, which is a claim to be measured against a Revit
    // export of an electrical model rather than assumed here.
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_StairsRailing",
        element_type: BimElementType::Railing,
    },
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_Columns",
        element_type: BimElementType::Column,
    },
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_StructuralColumns",
        element_type: BimElementType::Column,
    },
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_CurtainWallMullions",
        element_type: BimElementType::Member,
    },
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_CurtainWallPanels",
        element_type: BimElementType::Plate,
    },
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_Windows",
        element_type: BimElementType::Window,
    },
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_Doors",
        element_type: BimElementType::Door,
    },
    // Measured like the rows above it, on a second reference export - one
    // whose settings keep the category AR S1's drop. Revit writes
    // `IfcFurniture` for all 289 of its furnishings there and
    // `IfcFurnishingElement`, the supertype, for none.
    SourceMapping {
        class_name: Some("FamilyInstance"),
        category_name: "OST_Furniture",
        element_type: BimElementType::Furniture,
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

/// The `PredefinedType` an element type fixes, where it fixes one.
///
/// Almost none do: an entity's own kind is left at the `NOTDEFINED` its
/// schema declares, because the source says nothing about which kind of pipe
/// segment or which kind of door a thing is. Insulation is the exception, and
/// it is one because the entity alone does not carry the reading:
/// `IfcCovering` says something clads something else, and only the kind says
/// what sort of cladding. A covering of unstated kind would lose exactly what
/// the source stated.
///
/// The value is a member of the entity's own enumeration - see
/// `the_predefined_types_this_names_are_the_schema_members` in the IFC writer,
/// which checks that against the generated schema table.
#[must_use]
pub fn ifc_predefined_type(element_type: BimElementType) -> Option<&'static str> {
    match element_type {
        BimElementType::Insulation => Some("INSULATION"),
        _ => None,
    }
}

/// The element's own type where it has one, and what its source vocabulary
/// says otherwise.
///
/// A reader that classified the element already has the answer; one that
/// left it `Unknown` has not, and the fallback reads the class and category
/// the source stated. Both writers and the viewer scene ask this rather than
/// each keeping the fallback, which is how three copies of it came about.
#[must_use]
pub fn resolved_element_type(element: &BimElement) -> BimElementType {
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

/// The IFC entity a model element of this type is written as.
///
/// This is the one mapping `export-ifc` applies, published so that anything
/// else - a viewer, a report - can say what an element would become without
/// running the export and without keeping a second copy of the rules that
/// could drift from this one.
#[must_use]
pub fn ifc_entity_name(element_type: BimElementType) -> &'static str {
    match element_type {
        BimElementType::PipeSegment => "IFCPIPESEGMENT",
        BimElementType::PipeFitting => "IFCPIPEFITTING",
        BimElementType::DuctFitting => "IFCDUCTFITTING",
        BimElementType::Insulation => "IFCCOVERING",
        BimElementType::SanitaryTerminal => "IFCSANITARYTERMINAL",
        BimElementType::AirTerminal => "IFCAIRTERMINAL",
        BimElementType::FireSuppressionTerminal => "IFCFIRESUPPRESSIONTERMINAL",
        BimElementType::Alarm => "IFCALARM",
        BimElementType::CableCarrierFitting => "IFCCABLECARRIERFITTING",
        BimElementType::LightFixture => "IFCLIGHTFIXTURE",
        BimElementType::DuctSegment => "IFCDUCTSEGMENT",
        BimElementType::CableCarrierSegment => "IFCCABLECARRIERSEGMENT",
        // The two distribution supertypes are instantiable but, unlike the
        // typed leaves, declare no `PredefinedType` - which the schema table
        // says for them as it does for the rest.
        BimElementType::DistributionElement => "IFCDISTRIBUTIONELEMENT",
        BimElementType::DistributionFlowElement => "IFCDISTRIBUTIONFLOWELEMENT",
        BimElementType::Wall => "IFCWALL",
        BimElementType::Slab => "IFCSLAB",
        BimElementType::Roof => "IFCROOF",
        BimElementType::Stair => "IFCSTAIR",
        BimElementType::StairFlight => "IFCSTAIRFLIGHT",
        BimElementType::CurtainWall => "IFCCURTAINWALL",
        BimElementType::Railing => "IFCRAILING",
        BimElementType::Column => "IFCCOLUMN",
        BimElementType::Member => "IFCMEMBER",
        BimElementType::Plate => "IFCPLATE",
        BimElementType::Window => "IFCWINDOW",
        BimElementType::Door => "IFCDOOR",
        BimElementType::Furniture => "IFCFURNITURE",
        BimElementType::Unknown => "IFCBUILDINGELEMENTPROXY",
        // A space never reaches `push_element`: `push_elements` sends a
        // spatial type to `push_space`, whose attributes are a spatial
        // element's rather than an element's. The match must still be total,
        // and naming the entity is better than a panic.
        BimElementType::Space => "IFCSPACE",
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

    /// The categories a mechanical or electrical model is made of, which is
    /// most of what either holds.
    #[test]
    fn maps_the_categories_a_services_model_is_made_of() {
        for (category_name, expected) in [
            ("OST_DuctFitting", BimElementType::DuctFitting),
            ("OST_ConduitFitting", BimElementType::CableCarrierFitting),
            ("OST_LightingFixtures", BimElementType::LightFixture),
            // A device category names a discipline and not a device, so it
            // resolves to the supertype. Every one of Revit's reads alike.
            ("OST_LightingDevices", BimElementType::DistributionElement),
            (
                "OST_ElectricalFixtures",
                BimElementType::DistributionElement,
            ),
            ("OST_DataDevices", BimElementType::DistributionElement),
            (
                "OST_CommunicationDevices",
                BimElementType::DistributionElement,
            ),
            ("OST_SecurityDevices", BimElementType::DistributionElement),
            ("OST_NurseCallDevices", BimElementType::DistributionElement),
            ("OST_TelephoneDevices", BimElementType::DistributionElement),
            // Except the one IFC names, which keeps its own entity rather
            // than falling in with the supertype above.
            ("OST_FireAlarmDevices", BimElementType::Alarm),
        ] {
            assert_eq!(
                element_type_for_source(Some("FamilyInstance"), Some(category_name)),
                expected,
                "{category_name}"
            );
        }
    }

    /// A run of a building system states no category of its own on most of
    /// the records that carry one, so requiring a category loses most of a
    /// mechanical or electrical model: 5 432 pipe runs and 750 duct runs on
    /// one ventilation model of 22 783 elements, 1 355 conduit runs on one
    /// electrical model of 12 627. The class says what the run is on its own.
    #[test]
    fn types_a_building_system_run_from_its_class_alone() {
        for (class_name, expected) in [
            ("RbsPipeCurve", BimElementType::PipeSegment),
            ("RbsFlexPipeCurve", BimElementType::PipeSegment),
            ("RbsDuctCurve", BimElementType::DuctSegment),
            ("RbsFlexDuctCurve", BimElementType::DuctSegment),
            ("RbsConduitCurve", BimElementType::CableCarrierSegment),
            ("CableTray", BimElementType::CableCarrierSegment),
            ("RbsPipeInsulation", BimElementType::Insulation),
            ("RbsDuctInsulation", BimElementType::Insulation),
        ] {
            assert_eq!(element_type_for_source(Some(class_name), None), expected);
            // And the class agrees with the row it mirrors, which is what
            // makes it no new claim: the pair says the same as the class.
            let category = match expected {
                BimElementType::PipeSegment if class_name.contains("Flex") => "OST_FlexPipeCurves",
                BimElementType::PipeSegment => "OST_PipeCurves",
                BimElementType::DuctSegment if class_name.contains("Flex") => "OST_FlexDuctCurves",
                BimElementType::DuctSegment => "OST_DuctCurves",
                BimElementType::Insulation if class_name.contains("Pipe") => "OST_PipeInsulations",
                BimElementType::Insulation => "OST_DuctInsulations",
                _ if class_name == "CableTray" => "OST_CableTray",
                _ => "OST_Conduit",
            };
            assert_eq!(
                element_type_for_source(Some(class_name), Some(category)),
                expected
            );
        }
        // A run's class is not a licence to type anything else: the lining of
        // a duct is neither the duct nor its insulation, and stays unread.
        assert_eq!(
            element_type_for_source(Some("RbsDuctLining"), None),
            BimElementType::Unknown
        );
    }

    /// Insulation is an `IfcCovering`, and a covering says what it is only
    /// through its kind: without `INSULATION` the file would state that a run
    /// is clad in something unspecified.
    #[test]
    fn insulation_is_a_covering_of_a_stated_kind() {
        for (class_name, category_name) in [
            (Some("RbsPipeInsulation"), None),
            (Some("RbsDuctInsulation"), None),
            (Some("FamilyInstance"), Some("OST_PipeInsulations")),
            (Some("FamilyInstance"), Some("OST_DuctInsulations")),
        ] {
            let element_type = element_type_for_source(class_name, category_name);
            assert_eq!(element_type, BimElementType::Insulation, "{class_name:?}");
            assert_eq!(ifc_entity_name(element_type), "IFCCOVERING");
            assert_eq!(ifc_predefined_type(element_type), Some("INSULATION"));
        }
        // And it is the only type that fixes one: everything else leaves the
        // entity's own `NOTDEFINED` standing, because the source says nothing
        // about which kind of pipe segment or which kind of door a thing is.
        let fixed: Vec<BimElementType> = BimElementType::ALL
            .iter()
            .copied()
            .filter(|element_type| ifc_predefined_type(*element_type).is_some())
            .collect();
        assert_eq!(fixed, vec![BimElementType::Insulation]);
    }

    /// Every electrical category IFC has an entity for gets one, and the two
    /// that name a discipline rather than a device get the supertype.
    #[test]
    fn writes_the_electrical_entities_ifc_names() {
        for (element_type, entity) in [
            (BimElementType::LightFixture, "IFCLIGHTFIXTURE"),
            (
                BimElementType::CableCarrierSegment,
                "IFCCABLECARRIERSEGMENT",
            ),
            (
                BimElementType::CableCarrierFitting,
                "IFCCABLECARRIERFITTING",
            ),
            (BimElementType::DuctFitting, "IFCDUCTFITTING"),
            (BimElementType::Insulation, "IFCCOVERING"),
            (
                BimElementType::DistributionElement,
                "IFCDISTRIBUTIONELEMENT",
            ),
        ] {
            assert_eq!(ifc_entity_name(element_type), entity);
        }
        // A category nothing in the table names is still a proxy: the point
        // of the rows above is the ones that are named, not a rule that
        // guesses at the rest.
        assert_eq!(
            ifc_entity_name(element_type_for_source(
                Some("FamilyInstance"),
                Some("OST_SpecialityEquipment")
            )),
            "IFCBUILDINGELEMENTPROXY"
        );
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
            // Drawn as a system family, so it declares no category and the
            // loadable railing's `OST_StairsRailing` never reaches it.
            ("BaseRailing", BimElementType::Railing),
        ] {
            assert_eq!(element_type_for_source(Some(class_name), None), expected);
        }
        // Both readings of a railing land on the one entity.
        assert_eq!(
            ifc_entity_name(element_type_for_source(Some("BaseRailing"), None)),
            ifc_entity_name(element_type_for_source(
                Some("FamilyInstance"),
                Some("OST_StairsRailing")
            ))
        );
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

    /// The loadable-family rows, each measured against Revit's own export of
    /// AR S1 with no spread. See `SOURCE_MAPPINGS` for the counts.
    #[test]
    fn types_a_loadable_family_from_the_category_its_family_declares() {
        for (category_name, element_type) in [
            ("OST_StairsRailing", BimElementType::Railing),
            ("OST_Columns", BimElementType::Column),
            ("OST_StructuralColumns", BimElementType::Column),
            ("OST_CurtainWallMullions", BimElementType::Member),
            ("OST_CurtainWallPanels", BimElementType::Plate),
            ("OST_Windows", BimElementType::Window),
            ("OST_Doors", BimElementType::Door),
        ] {
            assert_eq!(
                element_type_for_source(Some("FamilyInstance"), Some(category_name)),
                element_type,
                "{category_name}"
            );
            // The class is required with it: these are a loadable family's
            // categories, and nothing establishes them for another class.
            assert_eq!(
                element_type_for_source(Some("SWall"), Some(category_name)),
                BimElementType::Unknown,
                "{category_name}"
            );
        }
    }

    /// Two categories the reference join deliberately leaves unmapped: Revit
    /// exports structural framing as a proxy, and turns a generic model into
    /// an opening, which is a void rather than a product.
    #[test]
    fn leaves_the_categories_revit_does_not_type_alone() {
        for category_name in ["OST_StructuralFraming", "OST_GenericModel"] {
            assert_eq!(
                element_type_for_source(Some("FamilyInstance"), Some(category_name)),
                BimElementType::Unknown,
                "{category_name}"
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
        // A building system's run used to be here too: its class typed it
        // only when its own record repeated the category. That was the
        // conservative reading while no mechanical or electrical model had
        // been looked at - and measured against three electrical models and
        // one ventilation model, it left most of each of them untyped, since
        // most such records declare no category at all. The class alone is
        // what types them now; see
        // `types_a_building_system_run_from_its_class_alone`.
        //
        // What stays refused is a class the table does not name. A duct's
        // lining is not the duct, nor the insulation wrapped round its
        // outside, and nothing types it from either.
        for class_name in ["RbsDuctLining", "CableTrayFitting"] {
            assert_eq!(
                element_type_for_source(Some(class_name), None),
                BimElementType::Unknown,
                "{class_name}"
            );
        }
    }
}

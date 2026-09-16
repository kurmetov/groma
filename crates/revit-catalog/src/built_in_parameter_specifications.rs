//! Which Forge spec a built-in parameter's stored double is measured in.
//!
//! Hand-maintained, unlike its neighbours. Autodesk's published
//! `BuiltInParameter` tables - the ones
//! `scripts/generate_revit_2023_catalog.py` scrapes - carry a code, an enum
//! name and a label, and no spec at all; the binding exists only behind
//! `ParameterUtils.GetParameterTypeId` at runtime. So each row here is read
//! off the enum name in `built_in_parameters_2023.rs`, which is generated and
//! is not invented here, and is taken from the published specification rather
//! than verified against a second implementation - the same standing the DWG
//! signature has, and for the same reason.
//!
//! A row is only worth adding where the enum name settles the quantity on its
//! own. A double whose parameter is not listed keeps reaching the exporter
//! unconverted and marked, which is the honest answer and not a silently
//! wrong number: on the 231 MB architectural model 125 distinct built-in
//! codes carry a double, and their quantities run from lengths through
//! angles, moduli, Poisson ratios and thermal expansion coefficients to text
//! style flags. Guessing "length" across that set would corrupt most of it.
//!
//! Ordered by code, for `binary_search_by_key`.

/// Forge spec identifiers, named once so a typo cannot differ between rows.
const LENGTH: &str = "autodesk.spec.aec:length-2.0.1";
const ANGLE: &str = "autodesk.spec.aec:angle-2.0.0";
const NUMBER: &str = "autodesk.spec.aec:number-2.0.0";
const THERMAL_RESISTANCE: &str = "autodesk.spec.aec.energy:thermalResistance-2.0.0";
const HEAT_TRANSFER_COEFFICIENT: &str = "autodesk.spec.aec.energy:heatTransferCoefficient-2.0.0";

pub(super) static VALUES: &[(i32, &str)] = &[
    // WALL_BOTTOM_EXTENSION_DIST_PARAM, WALL_TOP_EXTENSION_DIST_PARAM
    (-1_012_829, LENGTH),
    (-1_012_828, LENGTH),
    // MULLION_OFFSET, RECT_MULLION_THICK, RECT_MULLION_WIDTH2,
    // RECT_MULLION_WIDTH1
    (-1_007_351, LENGTH),
    (-1_007_304, LENGTH),
    (-1_007_301, LENGTH),
    (-1_007_300, LENGTH),
    // STRUCTURAL_SECTION_ISHAPE_FLANGETHICKNESS,
    // STRUCTURAL_SECTION_COMMON_HEIGHT, STRUCTURAL_SECTION_COMMON_WIDTH: the
    // dimensions of a structural section's profile, which are lengths in every
    // shape that declares them.
    (-1_005_524, LENGTH),
    (-1_005_503, LENGTH),
    (-1_005_502, LENGTH),
    // ANALYTICAL_VISUAL_LIGHT_TRANSMITTANCE,
    // ANALYTICAL_SOLAR_HEAT_GAIN_COEFFICIENT: both are ratios and neither
    // carries a dimension, which `aec:number` is the spec for.
    (-1_005_433, NUMBER),
    (-1_005_432, NUMBER),
    // ANALYTICAL_THERMAL_RESISTANCE, ANALYTICAL_HEAT_TRANSFER_COEFFICIENT
    (-1_005_431, THERMAL_RESISTANCE),
    (-1_005_430, HEAT_TRANSFER_COEFFICIENT),
    // FLOOR_PARAM_SPAN_DIRECTION: a direction in the slab's plane, stored in
    // radians as every Revit angle is.
    (-1_001_955, ANGLE),
    // FLOOR_HEIGHTABOVELEVEL_PARAM
    (-1_001_951, LENGTH),
    // REVOLUTION_END_ANGLE, REVOLUTION_START_ANGLE: the sweep a revolved form
    // turns through, in the radians every Revit angle is stored in.
    (-1_001_803, ANGLE),
    (-1_001_802, ANGLE),
    // EXTRUSION_END_PARAM, EXTRUSION_START_PARAM: where a form's extrusion
    // begins and ends along its own axis.
    (-1_001_801, LENGTH),
    (-1_001_800, LENGTH),
    // FASCIA_DEPTH_PARAM, ACTUAL_MAX_RIDGE_HEIGHT_PARAM,
    // ROOF_UPTO_LEVEL_OFFSET_PARAM, ROOF_LEVEL_OFFSET_PARAM
    (-1_001_711, LENGTH),
    (-1_001_705, LENGTH),
    (-1_001_703, LENGTH),
    (-1_001_701, LENGTH),
    // FAMILY_WPB_DEFAULT_ELEVATION, FAMILY_ROUGH_WIDTH_PARAM,
    // FAMILY_ROUGH_HEIGHT_PARAM, DOOR_THICKNESS, CASEWORK_WIDTH,
    // CASEWORK_HEIGHT. The last three are the generic family dimensions and
    // are carried by more than the family their enum name is written for.
    (-1_001_320, LENGTH),
    (-1_001_305, LENGTH),
    (-1_001_304, LENGTH),
    (-1_001_302, LENGTH),
    (-1_001_301, LENGTH),
    (-1_001_300, LENGTH),
    // WALL_TOP_OFFSET, WALL_BASE_OFFSET, WALL_USER_HEIGHT_PARAM
    (-1_001_109, LENGTH),
    (-1_001_108, LENGTH),
    (-1_001_105, LENGTH),
];

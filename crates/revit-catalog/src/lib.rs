#![forbid(unsafe_code)]

mod built_in_categories_2023;
mod built_in_parameter_specifications;
mod built_in_parameters_2023;
mod specifications_2023;

/// Metres in one Revit internal length unit (one international foot).
pub const METRES_PER_INTERNAL_FOOT: f64 = 0.3048;

/// One member of Autodesk's `BuiltInParameter` enumeration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltInParameter {
    pub code: i32,
    pub enum_name: &'static str,
    /// English label published with the enumeration when one is available.
    pub display_name: &'static str,
}

/// One member of Autodesk's `BuiltInCategory` enumeration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltInCategory {
    pub code: i32,
    pub enum_name: &'static str,
}

/// A Forge measurable spec and its canonical storage unit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Specification {
    pub type_id: &'static str,
    pub name: &'static str,
    pub storage_unit: &'static str,
    pub storage_unit_name: &'static str,
    /// `storage = internal * scale + offset`.
    pub internal_to_storage_scale: f64,
    /// `storage = internal * scale + offset`.
    pub internal_to_storage_offset: f64,
    key: &'static str,
}

impl Specification {
    /// Convert a finite Revit internal value to the Forge storage unit.
    #[must_use]
    pub fn from_internal(self, value: f64) -> Option<f64> {
        value
            .is_finite()
            .then(|| {
                value.mul_add(
                    self.internal_to_storage_scale,
                    self.internal_to_storage_offset,
                )
            })
            .filter(|converted| converted.is_finite())
    }
}

/// Versioned external catalog. Unsupported releases deliberately return no
/// names instead of silently applying a table from another Revit version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Catalog {
    release: u16,
}

impl Catalog {
    #[must_use]
    pub const fn for_release(release: u16) -> Option<Self> {
        match release {
            2023 => Some(Self { release }),
            _ => None,
        }
    }

    #[must_use]
    pub const fn release(self) -> u16 {
        self.release
    }

    #[must_use]
    pub fn built_in_parameter(self, code: i32) -> Option<&'static BuiltInParameter> {
        debug_assert_eq!(self.release, 2023);
        if code == -1 {
            return None;
        }
        find_by_code(built_in_parameters_2023::VALUES, code, |entry| entry.code)
    }

    #[must_use]
    pub fn built_in_category(self, code: i32) -> Option<&'static BuiltInCategory> {
        debug_assert_eq!(self.release, 2023);
        if code == -1 {
            return None;
        }
        find_by_code(built_in_categories_2023::VALUES, code, |entry| entry.code)
    }

    /// The Forge spec a built-in parameter's stored double is measured in,
    /// where that is settled. Nothing declares it in the file - a built-in
    /// parameter takes its spec from Revit's own definition of it - so this
    /// is the only way one of them reaches an exporter as a quantity rather
    /// than as a raw internal number.
    #[must_use]
    pub fn built_in_parameter_specification(self, code: i32) -> Option<&'static str> {
        debug_assert_eq!(self.release, 2023);
        find_by_code(built_in_parameter_specifications::VALUES, code, |entry| {
            entry.0
        })
        .map(|entry| entry.1)
    }

    /// Resolve a Forge spec while ignoring only its semantic-version suffix,
    /// matching `ForgeTypeId`'s version-insensitive identity convention.
    #[must_use]
    pub fn specification(self, type_id: &str) -> Option<&'static Specification> {
        debug_assert_eq!(self.release, 2023);
        let key = forge_type_key(type_id);
        specifications_2023::VALUES
            .binary_search_by_key(&key, |entry| entry.key)
            .ok()
            .map(|index| &specifications_2023::VALUES[index])
            .or_else(|| {
                // Autodesk spells this one spec two ways and files carry both:
                // `measurable` in the 1.0.0 the 231 MB architectural model
                // declares for its own shared parameters, `measurabLe` in the
                // 2.0.0 of the schema the table above is generated verbatim
                // from. Version-insensitivity does not bridge that, and the
                // generated file is not edited, so the alias lives here.
                (forge_type_key(type_id) == "autodesk.spec.measurable:currency")
                    .then(|| {
                        specifications_2023::VALUES
                            .binary_search_by_key(&"autodesk.spec.measurabLe:currency", |entry| {
                                entry.key
                            })
                            .ok()
                            .map(|index| &specifications_2023::VALUES[index])
                    })
                    .flatten()
            })
    }
}

/// Convert feet to metres for fields, such as level elevation, whose length
/// semantics are known from the owning class rather than a parameter spec.
#[must_use]
pub fn internal_feet_to_metres(value: f64) -> Option<f64> {
    value
        .is_finite()
        .then_some(value * METRES_PER_INTERNAL_FOOT)
        .filter(|converted| converted.is_finite())
}

fn find_by_code<T>(values: &'static [T], code: i32, key: impl Fn(&T) -> i32) -> Option<&'static T> {
    values
        .binary_search_by_key(&code, key)
        .ok()
        .map(|index| &values[index])
}

fn forge_type_key(type_id: &str) -> &str {
    let Some((key, version)) = type_id.rsplit_once('-') else {
        return type_id;
    };
    let mut parts = version.split('.');
    let is_version = parts.clone().count() == 3
        && parts.all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if is_version { key } else { type_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: Catalog = Catalog { release: 2023 };

    #[test]
    fn rejects_an_unsupported_release() {
        assert_eq!(Catalog::for_release(2026), None);
    }

    #[test]
    fn resolves_public_parameter_and_category_codes() {
        let parameter = CATALOG.built_in_parameter(-1_001_203).unwrap();
        assert_eq!(parameter.enum_name, "ALL_MODEL_MARK");
        assert!(!parameter.display_name.is_empty());

        let category = CATALOG.built_in_category(-2_000_011).unwrap();
        assert_eq!(category.enum_name, "OST_Walls");
        assert!(CATALOG.built_in_parameter(-1).is_none());
        assert!(CATALOG.built_in_category(-1).is_none());
    }

    #[test]
    fn converts_length_area_and_temperature() {
        let length = CATALOG
            .specification("autodesk.spec.aec:length-1.0.0")
            .unwrap();
        assert_eq!(length.storage_unit, "autodesk.unit.unit:meters-1.0.0");
        assert!((length.from_internal(10.0).unwrap() - 3.048).abs() < 1.0e-12);

        let area = CATALOG
            .specification("autodesk.spec.aec:area-2.0.0")
            .unwrap();
        assert!((area.from_internal(100.0).unwrap() - 9.290_304).abs() < 1.0e-12);

        let temperature = CATALOG
            .specification("autodesk.spec.aec.hvac:temperature-2.0.0")
            .unwrap();
        assert!((temperature.from_internal(293.15).unwrap() - 20.0).abs() < 1.0e-12);
    }

    #[test]
    fn rejects_non_finite_values() {
        assert_eq!(internal_feet_to_metres(f64::NAN), None);
        let length = CATALOG
            .specification("autodesk.spec.aec:length-2.0.0")
            .unwrap();
        assert_eq!(length.from_internal(f64::INFINITY), None);
    }

    #[test]
    fn gives_a_built_in_parameter_the_spec_nothing_in_the_file_states() {
        let length = CATALOG
            .built_in_parameter_specification(-1_001_105)
            .unwrap();
        assert_eq!(
            CATALOG.built_in_parameter(-1_001_105).unwrap().enum_name,
            "WALL_USER_HEIGHT_PARAM"
        );
        let resolved = CATALOG.specification(length).unwrap();
        assert_eq!(resolved.storage_unit, "autodesk.unit.unit:meters-1.0.0");
        // 13.123... internal feet is the 4 m the model was drawn with.
        assert!((resolved.from_internal(13.123_359_580_052_492).unwrap() - 4.0).abs() < 1.0e-12);

        // An angle is not a length, and a code the table does not claim stays
        // unclaimed rather than taking a neighbour's spec.
        assert_eq!(
            CATALOG.built_in_parameter_specification(-1_001_955),
            Some("autodesk.spec.aec:angle-2.0.0")
        );
        assert_eq!(CATALOG.built_in_parameter_specification(-1_001_111), None);
        assert_eq!(CATALOG.built_in_parameter_specification(-1), None);

        // A form states where its extrusion starts as a length and how far
        // its revolution turns as an angle. Both reach AR S1's products -
        // 124 and 4 values - and both used to leave unconverted.
        assert_eq!(
            CATALOG.built_in_parameter(-1_001_800).unwrap().enum_name,
            "EXTRUSION_START_PARAM"
        );
        assert_eq!(
            CATALOG.built_in_parameter_specification(-1_001_800),
            Some("autodesk.spec.aec:length-2.0.1")
        );
        assert_eq!(
            CATALOG.built_in_parameter(-1_001_802).unwrap().enum_name,
            "REVOLUTION_START_ANGLE"
        );
        assert_eq!(
            CATALOG.built_in_parameter_specification(-1_001_802),
            Some("autodesk.spec.aec:angle-2.0.0")
        );
        assert_eq!(
            CATALOG.built_in_parameter(-1_005_502).unwrap().enum_name,
            "STRUCTURAL_SECTION_COMMON_WIDTH"
        );
        assert_eq!(
            CATALOG.built_in_parameter_specification(-1_005_502),
            Some("autodesk.spec.aec:length-2.0.1")
        );
    }

    #[test]
    fn every_built_in_parameter_spec_is_ordered_named_and_resolvable() {
        let mut previous = i32::MIN;
        for (code, type_id) in built_in_parameter_specifications::VALUES {
            assert!(*code > previous, "codes must ascend for the binary search");
            previous = *code;
            assert!(
                CATALOG.built_in_parameter(*code).is_some(),
                "{code} names no published parameter"
            );
            assert!(
                CATALOG.specification(type_id).is_some(),
                "{type_id} names no published spec"
            );
        }
    }

    #[test]
    fn resolves_the_currency_spec_under_either_spelling() {
        // The identifier files carry, and the one the generated table holds.
        for type_id in [
            "autodesk.spec.measurable:currency-1.0.0",
            "autodesk.spec.measurabLe:currency-2.0.0",
        ] {
            assert_eq!(
                CATALOG.specification(type_id).unwrap().storage_unit,
                "autodesk.unit.unit:currency-1.0.0"
            );
        }
        assert!(CATALOG.specification("autodesk.spec.aec:nothing").is_none());
    }

    #[test]
    fn only_strips_a_semantic_version() {
        assert_eq!(forge_type_key("example-1.2.3"), "example");
        assert_eq!(forge_type_key("example-alpha"), "example-alpha");
        assert_eq!(forge_type_key("example-1.2"), "example-1.2");
    }
}

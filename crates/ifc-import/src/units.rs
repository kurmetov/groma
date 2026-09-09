//! The units a file states its numbers in.
//!
//! Nothing downstream of this crate carries a file's own unit: `bim-core`
//! geometry is in metres and angles are in radians, so every length and every
//! angle read from the file is multiplied here exactly once. A file that
//! declares no length unit is read as metres, which is what ISO 10303-21
//! leaves as the SI default - and is recorded in [`Units::stated_length`] so
//! a caller can say whether the scale was declared or assumed.

use crate::step::{Entity, Parsed, Value};

/// Multipliers from a file's own units into the ones this crate emits.
#[derive(Clone, Copy, Debug)]
pub struct Units {
    /// Metres per file length unit.
    pub length: f64,
    /// Radians per file plane-angle unit.
    pub angle: f64,
    /// Whether the file declared a length unit, rather than this falling back
    /// to the SI metre.
    pub stated_length: bool,
}

impl Default for Units {
    fn default() -> Self {
        Self {
            length: 1.0,
            angle: 1.0,
            stated_length: false,
        }
    }
}

impl Units {
    /// Read the unit assignment `IFCPROJECT` names.
    #[must_use]
    pub fn read(parsed: &Parsed) -> Self {
        let mut units = Self::default();
        let Some(assignment) = parsed
            .of_type("IFCPROJECT")
            .first()
            // `UnitsInContext` is IfcProject's ninth attribute.
            .and_then(|(_, project)| parsed.follow(project.attribute(8)))
            .or_else(|| {
                parsed
                    .of_type("IFCUNITASSIGNMENT")
                    .first()
                    .map(|(_, entity)| *entity)
            })
        else {
            return units;
        };
        for member in assignment
            .attribute(0)
            .and_then(Value::as_list)
            .unwrap_or_default()
        {
            let Some(unit) = parsed.follow(Some(member)) else {
                continue;
            };
            // `UnitType` is the second attribute of both named-unit forms.
            match unit.attribute(1).and_then(Value::as_enumeration) {
                Some("LENGTHUNIT") => {
                    if let Some(scale) = scale_of(parsed, unit, 0) {
                        units.length = scale;
                        units.stated_length = true;
                    }
                }
                Some("PLANEANGLEUNIT") => {
                    if let Some(scale) = scale_of(parsed, unit, 0) {
                        units.angle = scale;
                    }
                }
                _ => {}
            }
        }
        units
    }
}

/// How many SI base units one of `unit` is.
///
/// `depth` bounds the conversion chain a file may state: a conversion-based
/// unit names another unit, which a malformed file could point back at.
fn scale_of(parsed: &Parsed, unit: &Entity, depth: u8) -> Option<f64> {
    if depth > 4 {
        return None;
    }
    match unit.type_name.as_str() {
        // The metre and the radian are the SI units themselves; a prefix
        // scales them, and only the metre takes one in practice.
        "IFCSIUNIT" => Some(prefix_scale(
            unit.attribute(2).and_then(Value::as_enumeration),
        )),
        // `ConversionFactor` is an `IfcMeasureWithUnit`: a number in the unit
        // its second attribute names.
        "IFCCONVERSIONBASEDUNIT" | "IFCCONVERSIONBASEDUNITWITHOFFSET" => {
            let factor = parsed.follow(unit.attribute(3))?;
            let value = factor.attribute(0).and_then(Value::as_number)?;
            let base = parsed.follow(factor.attribute(1))?;
            Some(value * scale_of(parsed, base, depth + 1)?)
        }
        _ => None,
    }
}

fn prefix_scale(prefix: Option<&str>) -> f64 {
    match prefix {
        Some("EXA") => 1e18,
        Some("PETA") => 1e15,
        Some("TERA") => 1e12,
        Some("GIGA") => 1e9,
        Some("MEGA") => 1e6,
        Some("KILO") => 1e3,
        Some("HECTO") => 1e2,
        Some("DECA") => 1e1,
        Some("DECI") => 1e-1,
        Some("CENTI") => 1e-2,
        Some("MILLI") => 1e-3,
        Some("MICRO") => 1e-6,
        Some("NANO") => 1e-9,
        Some("PICO") => 1e-12,
        Some("FEMTO") => 1e-15,
        Some("ATTO") => 1e-18,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::Units;
    use crate::step::parse;

    fn units_of(data: &str) -> Units {
        let text = format!("ISO-10303-21;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n");
        Units::read(&parse(text.as_bytes()).expect("a STEP file"))
    }

    #[test]
    fn reads_millimetres_and_degrees_the_way_revit_writes_them() {
        let units = units_of(
            "#1=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);\n\
             #2=IFCSIUNIT(*,.PLANEANGLEUNIT.,$,.RADIAN.);\n\
             #3=IFCMEASUREWITHUNIT(IFCPLANEANGLEMEASURE(0.017453292519943295),#2);\n\
             #4=IFCCONVERSIONBASEDUNIT(#9,.PLANEANGLEUNIT.,'DEGREE',#3);\n\
             #5=IFCUNITASSIGNMENT((#1,#4));\n\
             #6=IFCPROJECT('g',$,'p',$,$,$,$,(),#5);\n",
        );
        assert!((units.length - 0.001).abs() < 1e-12);
        assert!((units.angle - std::f64::consts::PI / 180.0).abs() < 1e-12);
        assert!(units.stated_length);
    }

    #[test]
    fn falls_back_to_the_si_metre_and_says_so() {
        let units = units_of("#1=IFCWALL('a',$,$,$,$,$,$,$,$);\n");
        assert!((units.length - 1.0).abs() < 1e-12);
        assert!(!units.stated_length);
    }

    #[test]
    fn reads_the_foot_a_conversion_factor_states() {
        let units = units_of(
            "#1=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);\n\
             #2=IFCMEASUREWITHUNIT(IFCLENGTHMEASURE(0.3048),#1);\n\
             #3=IFCCONVERSIONBASEDUNIT(#9,.LENGTHUNIT.,'FOOT',#2);\n\
             #4=IFCUNITASSIGNMENT((#3));\n\
             #5=IFCPROJECT('g',$,'p',$,$,$,$,(),#4);\n",
        );
        assert!((units.length - 0.3048).abs() < 1e-12);
        assert!(units.stated_length);
    }
}

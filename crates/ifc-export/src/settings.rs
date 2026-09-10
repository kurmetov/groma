//! What an export is allowed to decide, held in one place.
//!
//! The shape follows Revit's own *IFC Export* setup, because that is the
//! vocabulary the people who will read our files already have: a view
//! definition, a length unit, which property sets to write, whether types,
//! openings and base quantities come with them. A setting appears here only
//! when the exporter honours it - an option that is written down and then
//! ignored is worse than one that was never offered.
//!
//! Everything has a default, so a caller that says nothing gets the export
//! this project shipped before settings existed, with the one exception the
//! header change below records.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::class_mapping::{ClassMapping, ClassMappingError};

/// The model view the file claims in its `FILE_DESCRIPTION`.
///
/// This is not decoration. Reference View forbids the B-Rep forms that carry
/// our geometry - it is a tessellated, reference-only exchange - and until the
/// exporter can tessellate, claiming it is a false statement about the file.
/// The default is therefore Design Transfer View, which is what the entities
/// we write actually belong to; the header said `ReferenceView_V1.2` before
/// this existed, over `IfcAdvancedBrep` bodies that the view does not allow.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewDefinition {
    /// `DesignTransferView_V1.0`: solids, B-Reps and swept solids.
    #[default]
    DesignTransfer,
}

impl ViewDefinition {
    /// The string IFC4 gives this view in a `FILE_DESCRIPTION`.
    #[must_use]
    pub const fn view_definition(self) -> &'static str {
        match self {
            Self::DesignTransfer => "DesignTransferView_V1.0",
        }
    }
}

/// The unit every length in the file is written in.
///
/// Revit offers the project's own display unit; a millimetre model is what its
/// export of the corpus writes, and a metre file is what this exporter wrote
/// before the setting existed. Both are the same model - IFC states its unit -
/// so this changes the numbers and nothing else.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LengthUnit {
    #[default]
    Metre,
    Millimetre,
}

impl LengthUnit {
    /// How many of this unit make up one metre. Every length the model carries
    /// is in metres, so this is the factor applied on the way out.
    #[must_use]
    pub const fn per_metre(self) -> f64 {
        match self {
            Self::Metre => 1.0,
            Self::Millimetre => 1000.0,
        }
    }

    /// The `IfcSIUnit` prefix this unit needs, if any.
    #[must_use]
    pub const fn si_prefix(self) -> Option<&'static str> {
        match self {
            Self::Metre => None,
            Self::Millimetre => Some("MILLI"),
        }
    }
}

impl fmt::Display for LengthUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Metre => "metre",
            Self::Millimetre => "millimetre",
        })
    }
}

/// Which property sets accompany an element. Revit's *Property Sets* tab, with
/// the two Revit-parameter switches kept apart because the sets themselves are:
/// one holds what the element declares, the other what its type does.
// One field per switch, as the dialog has one checkbox per switch.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct PropertySetSettings {
    /// The element's own Revit parameters, as `Rivet Properties`.
    pub revit_parameters: bool,
    /// The parameters the element's type carries, as `Rivet Type Properties`.
    pub revit_type_parameters: bool,
    /// IFC's own `Pset_..Common` for each element, which Revit exports too.
    ///
    /// Only `Reference` goes in it, and only because it is established:
    /// joined to the IFC Revit itself exported from AR S1 on the Revit element
    /// id, Revit's `Reference` is our recovered type name for 11 545 of the
    /// 11 895 products that carry one - every wall but two of 7 617, and every
    /// column, member, plate, railing, proxy, curtain wall and roof. The rest
    /// of what Revit puts in those sets is *not* established: no parameter
    /// this decode recovers agrees with `IsExternal`, `LoadBearing` or
    /// `ExtendToStructure` on any element where they vary, so none of them is
    /// written rather than guessed.
    pub ifc_common: bool,
    /// IFC's own `Qto_..BaseQuantities`, measured from the solid this file
    /// carries - Revit's *Export base quantities*, and off by default as it
    /// is there.
    ///
    /// Only `NetVolume` and `NetSurfaceArea`, and only for a closed shell of
    /// planar faces: a curved face would have to be tessellated, and a
    /// tessellation is an approximation whose error nothing here bounds.
    /// Measured against Revit's own export of AR S1 on the 8 047 solids both
    /// files hold, 7 482 reproduce its `NetVolume` to within a thousandth.
    pub base_quantities: bool,
}

impl Default for PropertySetSettings {
    fn default() -> Self {
        Self {
            revit_parameters: true,
            revit_type_parameters: true,
            ifc_common: true,
            base_quantities: false,
        }
    }
}

/// What the file says about the project it describes. Revit's *Project
/// Address* and the project information beside it: a name for the spatial
/// tree's three roots, the phase the model is in, and the postal address.
///
/// Every field is optional and nothing is invented. Where one is unset the
/// exporter keeps what it wrote before this existed - the source file's own
/// stem for the project, and `Site` and `Building` for the two below it -
/// because none of the three is recoverable from a decoded model today.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProjectSettings {
    pub name: Option<String>,
    /// `IfcProject.LongName`: the descriptive name beside the short one.
    pub long_name: Option<String>,
    /// `IfcProject.Phase`, which Revit fills from the project information.
    pub phase: Option<String>,
    pub site_name: Option<String>,
    pub building_name: Option<String>,
    /// The building's postal address, line by line, as `IfcPostalAddress`
    /// holds it. Revit writes one on the building too.
    pub address_lines: Vec<String>,
    pub town: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
}

impl ProjectSettings {
    /// Whether anything of an address was given. An `IfcPostalAddress` with
    /// nothing in it says less than no address at all.
    #[must_use]
    pub fn has_address(&self) -> bool {
        !self.address_lines.is_empty()
            || self.town.is_some()
            || self.region.is_some()
            || self.postal_code.is_some()
            || self.country.is_some()
    }
}

/// One export's settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ExportSettings {
    pub view_definition: ViewDefinition,
    pub length_unit: LengthUnit,
    pub property_sets: PropertySetSettings,
    /// Write an `IfcTypeProduct` for each Revit type and relate its elements
    /// to it, as Revit's own export does. The type is also where the type's
    /// parameters go: they hold for every element of it, so writing them on
    /// each of thousands of products repeats them thousands of times.
    pub types: bool,
    /// A class mapping table, in the tab-separated form Revit's *IFC Options*
    /// dialog reads and writes. It decides what an element of a mapped
    /// category is written as, ahead of the built-in mapping, and may keep a
    /// category out of the file altogether. See [`ClassMapping`].
    pub class_mapping_file: Option<PathBuf>,
    /// The table itself, once read. Not part of the saved setup: the setup
    /// names the file, and the file is where the rows live.
    #[serde(skip)]
    class_mapping: Option<ClassMapping>,
    pub project: ProjectSettings,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            view_definition: ViewDefinition::default(),
            length_unit: LengthUnit::default(),
            property_sets: PropertySetSettings::default(),
            types: true,
            class_mapping_file: None,
            class_mapping: None,
            project: ProjectSettings::default(),
        }
    }
}

/// Why a settings file could not be used.
#[derive(Debug)]
pub enum SettingsError {
    Read { path: String, source: io::Error },
    Parse { path: String, message: String },
    Write { path: String, source: io::Error },
    ClassMapping(ClassMappingError),
}

impl From<ClassMappingError> for SettingsError {
    fn from(error: ClassMappingError) -> Self {
        Self::ClassMapping(error)
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(formatter, "cannot read {path}: {source}"),
            Self::Parse { path, message } => {
                write!(formatter, "{path} is not a valid settings file: {message}")
            }
            Self::Write { path, source } => write!(formatter, "cannot write {path}: {source}"),
            Self::ClassMapping(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::ClassMapping(error) => Some(error),
            Self::Parse { .. } => None,
        }
    }
}

impl ExportSettings {
    /// Read a settings file. An unknown key is an error rather than something
    /// quietly dropped: a misspelled setting that changes nothing, silently,
    /// is the failure this refuses.
    ///
    /// # Errors
    ///
    /// Returns the read or parse failure, naming the file.
    pub fn from_json_file(path: &Path) -> Result<Self, SettingsError> {
        let text = fs::read_to_string(path).map_err(|source| SettingsError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut settings: Self =
            serde_json::from_str(&text).map_err(|error| SettingsError::Parse {
                path: path.display().to_string(),
                message: error.to_string(),
            })?;
        // A setup that names a mapping table is not honoured until the table
        // is read, and a setup silently not honoured is the failure this whole
        // module exists to avoid.
        settings.load_class_mapping()?;
        Ok(settings)
    }

    /// Read the class mapping table this setup names, if any. Called for a
    /// setup read from a file; a caller that sets the path itself calls it.
    ///
    /// # Errors
    ///
    /// Returns the read failure, or the first row that cannot be applied.
    pub fn load_class_mapping(&mut self) -> Result<(), SettingsError> {
        self.class_mapping = match self.class_mapping_file.as_deref() {
            Some(path) => Some(ClassMapping::from_file(path)?),
            None => None,
        };
        Ok(())
    }

    /// The class mapping table, once read.
    #[must_use]
    pub const fn class_mapping(&self) -> Option<&ClassMapping> {
        self.class_mapping.as_ref()
    }

    /// Use this table, rather than reading one from a file. For a caller that
    /// has the rows already - a server holding one setup for many exports, or
    /// a test.
    pub fn set_class_mapping(&mut self, mapping: ClassMapping) {
        self.class_mapping = Some(mapping);
    }

    /// Write these settings out, so a run can be repeated exactly.
    ///
    /// # Errors
    ///
    /// Returns the write failure, naming the file.
    pub fn to_json_file(&self, path: &Path) -> Result<(), SettingsError> {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_default();
        text.push('\n');
        fs::write(path, text).map_err(|source| SettingsError::Write {
            path: path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_export_this_project_already_wrote() {
        let settings = ExportSettings::default();
        assert_eq!(settings.length_unit, LengthUnit::Metre);
        assert!(settings.property_sets.revit_parameters);
        assert!(settings.property_sets.revit_type_parameters);
        assert!(settings.property_sets.ifc_common);
        // Off, as Revit's own *Export base quantities* is.
        assert!(!settings.property_sets.base_quantities);
        assert!(settings.types);
        // The one deliberate change: the header no longer claims a view the
        // bodies we write are not allowed in.
        assert_eq!(
            settings.view_definition.view_definition(),
            "DesignTransferView_V1.0"
        );
    }

    #[test]
    fn a_settings_file_names_only_what_it_changes() {
        let settings: ExportSettings =
            serde_json::from_str(r#"{"length-unit":"millimetre"}"#).expect("valid settings");
        assert_eq!(settings.length_unit, LengthUnit::Millimetre);
        assert_eq!(settings.property_sets, PropertySetSettings::default());
    }

    #[test]
    fn a_misspelled_setting_is_refused_rather_than_ignored() {
        let error = serde_json::from_str::<ExportSettings>(r#"{"length_unit":"millimetre"}"#)
            .expect_err("unknown key");
        assert!(error.to_string().contains("length_unit"), "{error}");
    }

    #[test]
    fn settings_survive_a_round_trip_through_a_file() {
        let settings = ExportSettings {
            length_unit: LengthUnit::Millimetre,
            property_sets: PropertySetSettings {
                revit_parameters: false,
                revit_type_parameters: true,
                ifc_common: false,
                base_quantities: true,
            },
            ..ExportSettings::default()
        };
        let text = serde_json::to_string(&settings).expect("serializable");
        let parsed: ExportSettings = serde_json::from_str(&text).expect("valid settings");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn a_millimetre_is_a_thousandth_of_the_unit_the_model_carries() {
        assert!((LengthUnit::Millimetre.per_metre() - 1000.0).abs() < f64::EPSILON);
        assert_eq!(LengthUnit::Millimetre.si_prefix(), Some("MILLI"));
        assert!((LengthUnit::Metre.per_metre() - 1.0).abs() < f64::EPSILON);
        assert_eq!(LengthUnit::Metre.si_prefix(), None);
    }
}

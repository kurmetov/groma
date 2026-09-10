//! Revit's IFC class mapping table, as a file this exporter reads.
//!
//! Revit keeps the rule "a category of this kind becomes that IFC class" in a
//! tab-separated text file, edited through *File > Import/Export Settings >
//! IFC Options*, and its export table has four columns:
//!
//! ```text
//! Category<TAB>Subcategory<TAB>IFC class name<TAB>IFC type
//! ```
//!
//! This reads that file and lets it decide what an element is written as,
//! ahead of the built-in mapping in [`crate::mapping`]. Two things differ from
//! Revit's own file, and both are deliberate:
//!
//! - The category is named by its `BuiltInCategory` - `OST_Walls`, or the
//!   number `-2000011` - rather than by the label Revit shows a user in their
//!   own language. That is what a decoded model carries: a category reaches
//!   this exporter as an enumeration member and an integer, and nothing in the
//!   file being read gives its Russian or English display name. Matching on a
//!   label we do not have would be matching on nothing.
//! - A row that names a subcategory is refused rather than quietly applied to
//!   the whole category, because this decode does not recover subcategories
//!   and cannot tell whether the row applies.
//!
//! `Not Exported` in the class column keeps its Revit meaning: elements of
//! that category are left out of the file.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::ifc4_entities::{IFC4_ELEMENTS, Ifc4Entity};

/// What a mapped category becomes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mapped {
    /// Revit's `Not Exported`: the element is left out of the file.
    NotExported,
    /// The IFC entity to write, upper-cased as STEP wants it, and the
    /// `PredefinedType` member the row asked for.
    Entity {
        name: String,
        predefined_type: Option<String>,
    },
}

/// One loaded mapping table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClassMapping {
    /// By the category as written, upper-cased so `ost_walls` and `OST_Walls`
    /// are the same key. A numeric `BuiltInCategory` is held as its digits.
    rows: BTreeMap<String, Mapped>,
}

/// Why a mapping table could not be used. Every one of these names the line,
/// because a table is edited by hand and a silent misreading of one row is a
/// model exported as something nobody asked for.
#[derive(Debug, Eq, PartialEq)]
pub enum ClassMappingError {
    Read {
        path: PathBuf,
        message: String,
    },
    Columns {
        line: usize,
        found: usize,
    },
    UnknownEntity {
        line: usize,
        name: String,
    },
    /// The file is Revit's *import* mapping - IFC class to Revit category -
    /// which is the other direction. Worth saying outright: the two tables
    /// look alike, they are both called an IFC class mapping, and the one a
    /// project has to hand is usually the import one.
    ImportTable {
        line: usize,
        entity: String,
    },
    UnknownPredefinedType {
        line: usize,
        entity: String,
        value: String,
        members: Vec<&'static str>,
    },
    PredefinedTypeNotDeclared {
        line: usize,
        entity: String,
    },
    Subcategory {
        line: usize,
        name: String,
    },
    DuplicateCategory {
        line: usize,
        name: String,
    },
}

impl fmt::Display for ClassMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(formatter, "cannot read {}: {message}", path.display())
            }
            Self::Columns { line, found } => write!(
                formatter,
                "line {line}: a mapping row is category, subcategory, IFC class \
                 and IFC type, tab separated - found {found} column(s)"
            ),
            Self::UnknownEntity { line, name } => write!(
                formatter,
                "line {line}: {name} is not an IFC4 element entity this exporter can write"
            ),
            Self::ImportTable { line, entity } => write!(
                formatter,
                "line {line} names {entity} in the first column, so this is Revit's IFC \
                 *import* mapping - IFC class to Revit category. The export table is the \
                 other way round: the Revit category first, then an empty subcategory \
                 column, then the IFC class to write it as"
            ),
            Self::UnknownPredefinedType {
                line,
                entity,
                value,
                members,
            } => write!(
                formatter,
                "line {line}: {entity} has no predefined type {value}; the schema allows {}",
                members.join(", ")
            ),
            Self::PredefinedTypeNotDeclared { line, entity } => write!(
                formatter,
                "line {line}: {entity} declares no predefined type, so it cannot be given one"
            ),
            Self::Subcategory { line, name } => write!(
                formatter,
                "line {line}: this decode does not recover subcategories, so the row for \
                 subcategory {name} cannot be applied - leave the column empty to map the \
                 whole category"
            ),
            Self::DuplicateCategory { line, name } => {
                write!(formatter, "line {line}: {name} is mapped twice")
            }
        }
    }
}

impl std::error::Error for ClassMappingError {}

impl ClassMapping {
    /// Read a mapping table from a Revit-shaped tab-separated file.
    ///
    /// # Errors
    ///
    /// Returns the read failure, or the first row that cannot be applied.
    pub fn from_file(path: &Path) -> Result<Self, ClassMappingError> {
        let text =
            fs::read_to_string(path).map_err(|error: io::Error| ClassMappingError::Read {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        Self::from_table(&text)
    }

    /// Read a mapping table from its text.
    ///
    /// # Errors
    ///
    /// Returns the first row that cannot be applied.
    pub fn from_table(text: &str) -> Result<Self, ClassMappingError> {
        let mut rows: BTreeMap<String, Mapped> = BTreeMap::new();
        // Revit writes these files as UTF-8 with a byte order mark and CRLF
        // line endings, and the mark is on the front of the first line - which
        // is a comment, and would not be read as one with the mark still on it.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.trim().is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let columns = trimmed.split('\t').collect::<Vec<_>>();
            // Revit writes a trailing tab, so a row may carry an empty fifth
            // column; three columns is the same row with the type left off.
            let columns = match columns.as_slice() {
                [category, subcategory, class] => [*category, *subcategory, *class, ""],
                [category, subcategory, class, ifc_type, rest @ ..]
                    if rest.iter().all(|column| column.trim().is_empty()) =>
                {
                    [*category, *subcategory, *class, *ifc_type]
                }
                found => {
                    return Err(ClassMappingError::Columns {
                        line: line_number,
                        found: found.len(),
                    });
                }
            };
            let [category, subcategory, class, ifc_type] = columns.map(str::trim);
            if category.is_empty() {
                return Err(ClassMappingError::Columns {
                    line: line_number,
                    found: 0,
                });
            }
            if !subcategory.is_empty() {
                return Err(ClassMappingError::Subcategory {
                    line: line_number,
                    name: subcategory.to_owned(),
                });
            }
            let mapped = if class.eq_ignore_ascii_case("Not Exported")
                || class.eq_ignore_ascii_case("NotExported")
            {
                Mapped::NotExported
            } else {
                let name = class.to_ascii_uppercase();
                let entity = entity_named(&name).ok_or_else(|| {
                    // The import table has the IFC class first and the Revit
                    // category where this expects one, so it fails here on
                    // every row. Say which file it is rather than which row.
                    if category.to_ascii_uppercase().starts_with("IFC")
                        && entity_named(&category.to_ascii_uppercase()).is_some()
                    {
                        return ClassMappingError::ImportTable {
                            line: line_number,
                            entity: category.to_owned(),
                        };
                    }
                    ClassMappingError::UnknownEntity {
                        line: line_number,
                        name: class.to_owned(),
                    }
                })?;
                let predefined_type = if ifc_type.is_empty() {
                    None
                } else {
                    let value = ifc_type.to_ascii_uppercase();
                    let declared = entity.predefined_type.ok_or_else(|| {
                        ClassMappingError::PredefinedTypeNotDeclared {
                            line: line_number,
                            entity: name.clone(),
                        }
                    })?;
                    if !declared.members.contains(&value.as_str()) {
                        return Err(ClassMappingError::UnknownPredefinedType {
                            line: line_number,
                            entity: name.clone(),
                            value,
                            members: declared.members.to_vec(),
                        });
                    }
                    Some(value)
                };
                Mapped::Entity {
                    name,
                    predefined_type,
                }
            };
            let key = category.to_ascii_uppercase();
            if rows.insert(key, mapped).is_some() {
                return Err(ClassMappingError::DuplicateCategory {
                    line: line_number,
                    name: category.to_owned(),
                });
            }
        }
        Ok(Self { rows })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// What this table says an element of `category` becomes, by the category's
    /// `BuiltInCategory` name or by its number - either may be written in the
    /// file, and a row matches whichever the element carries.
    #[must_use]
    pub fn lookup(&self, name: Option<&str>, id: Option<&str>) -> Option<&Mapped> {
        name.map(str::to_ascii_uppercase)
            .and_then(|name| self.rows.get(&name))
            .or_else(|| {
                id.map(str::to_ascii_uppercase)
                    .and_then(|id| self.rows.get(&id))
            })
    }
}

fn entity_named(name: &str) -> Option<&'static Ifc4Entity> {
    IFC4_ELEMENTS.iter().find(|entity| entity.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
# IFC Class Name and Type to Revit Category table
OST_Walls\t\tIfcWall\t
OST_StructuralFraming\t\tIfcBeam\tBEAM
-2000011x\t\tNot Exported\t
OST_GenericModel\t\tIFCBUILDINGELEMENTPROXY\tUSERDEFINED
";

    #[test]
    fn reads_a_revit_shaped_table() {
        let mapping = ClassMapping::from_table(TABLE).expect("a readable table");
        assert_eq!(mapping.len(), 4);
        assert_eq!(
            mapping.lookup(Some("OST_Walls"), None),
            Some(&Mapped::Entity {
                name: "IFCWALL".to_owned(),
                predefined_type: None,
            })
        );
        // The class and the type are matched without regard to case, which is
        // how a hand-edited table is actually written.
        assert_eq!(
            mapping.lookup(Some("ost_structuralframing"), None),
            Some(&Mapped::Entity {
                name: "IFCBEAM".to_owned(),
                predefined_type: Some("BEAM".to_owned()),
            })
        );
        assert_eq!(
            mapping.lookup(Some("-2000011X"), None),
            Some(&Mapped::NotExported)
        );
        // A category may be named by its number instead.
        assert_eq!(
            mapping.lookup(Some("OST_Nothing"), Some("-2000011x")),
            Some(&Mapped::NotExported)
        );
        assert_eq!(mapping.lookup(Some("OST_Ceilings"), None), None);
    }

    #[test]
    fn refuses_a_class_the_schema_does_not_have() {
        let error = ClassMapping::from_table("OST_Walls\t\tIfcWallish\t").expect_err("unknown");
        assert_eq!(
            error,
            ClassMappingError::UnknownEntity {
                line: 1,
                name: "IfcWallish".to_owned(),
            }
        );
        assert!(error.to_string().contains("IfcWallish"));
    }

    #[test]
    fn refuses_a_predefined_type_the_entity_does_not_declare() {
        let error =
            ClassMapping::from_table("OST_Walls\t\tIfcWall\tMULLION").expect_err("wrong member");
        match error {
            ClassMappingError::UnknownPredefinedType { entity, value, .. } => {
                assert_eq!(entity, "IFCWALL");
                assert_eq!(value, "MULLION");
            }
            other => panic!("{other}"),
        }
        // A wall does declare one, so the failure is about the member. An
        // entity that declares none says that instead.
        let error = ClassMapping::from_table("OST_Walls\t\tIfcDistributionElement\tPIPE")
            .expect_err("no predefined type");
        assert_eq!(
            error,
            ClassMappingError::PredefinedTypeNotDeclared {
                line: 1,
                entity: "IFCDISTRIBUTIONELEMENT".to_owned(),
            }
        );
    }

    #[test]
    fn refuses_a_row_this_decode_cannot_apply() {
        let error =
            ClassMapping::from_table("OST_Stairs\tLandings\tIfcSlab\t").expect_err("a subcategory");
        assert_eq!(
            error,
            ClassMappingError::Subcategory {
                line: 1,
                name: "Landings".to_owned(),
            }
        );
        let error = ClassMapping::from_table("OST_Walls\tIfcWall").expect_err("two columns");
        assert_eq!(error, ClassMappingError::Columns { line: 1, found: 2 });
        let error = ClassMapping::from_table("OST_Walls\t\tIfcWall\t\nOST_Walls\t\tIfcSlab\t")
            .expect_err("mapped twice");
        assert_eq!(
            error,
            ClassMappingError::DuplicateCategory {
                line: 2,
                name: "OST_Walls".to_owned(),
            }
        );
    }

    /// Revit's *import* mapping is the other direction, and it is the file a
    /// project usually has. Failing on it row by row would say nothing; this
    /// says which file it is.
    #[test]
    fn recognises_revits_import_mapping_and_says_so() {
        let error = ClassMapping::from_table(
            "# IFC Class Name and Type to Revit Category/Sub-Category Table\n\
             IfcWall\t\t\u{421}\u{442}\u{435}\u{43d}\u{44b}\t\n",
        )
        .expect_err("the import direction");
        assert_eq!(
            error,
            ClassMappingError::ImportTable {
                line: 2,
                entity: "IfcWall".to_owned(),
            }
        );
        assert!(error.to_string().contains("import"), "{error}");
    }

    #[test]
    fn a_comment_and_a_blank_line_are_not_rows() {
        let mapping = ClassMapping::from_table("# a comment\n\n   \nOST_Walls\t\tIfcWall\t\n")
            .expect("a readable table");
        assert_eq!(mapping.len(), 1);
        assert!(!mapping.is_empty());
    }

    /// The file Revit actually writes: UTF-8 with a byte order mark, CRLF
    /// endings, and a comment header. Read from the real
    /// `importIFCClassMapping.txt`, which is what a project has to hand.
    #[test]
    fn reads_the_file_revit_actually_writes() {
        let mapping = ClassMapping::from_table(
            "\u{feff}# IFC Class Name and Type table\r\nOST_Walls\t\tIfcWall\t\r\n",
        )
        .expect("a readable table");
        assert_eq!(mapping.len(), 1);
        assert_eq!(
            mapping.lookup(Some("OST_Walls"), None),
            Some(&Mapped::Entity {
                name: "IFCWALL".to_owned(),
                predefined_type: None,
            })
        );
    }
}

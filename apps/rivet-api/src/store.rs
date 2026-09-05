//! The queryable side of a model: load one `export-json` artefact, keep it in
//! memory, and answer the questions an agent asks of it.
//!
//! The store deliberately does no decoding of its own. It reads what
//! `rivet export-json` produced, so anything it reports is traceable to that
//! artefact and to the `source` record inside it, and a decode improvement
//! reaches the API by re-running the export rather than by changing this.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One parameter value as `export-json` wrote it.
///
/// A numeric value is in Revit's internal units (feet, square feet, radians)
/// unless `storage_value` is present, which happens only where the parameter's
/// Forge spec was recovered - for project parameters, not for built-in ones.
/// The raw value is always kept so a consumer is never handed a converted
/// number without being able to see what it came from.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Parameter {
    pub id: i64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_in: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub int: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_name: Option<String>,
}

impl Parameter {
    /// The value as text, for a free-text search and for an index document.
    fn as_search_text(&self) -> Option<String> {
        self.text.clone()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Source {
    pub partition: String,
    pub member: u64,
    pub offset: u64,
}

/// One element, mirroring the `export-json` line. Unknown fields are kept so
/// a consumer sees everything the export wrote, not a subset this crate
/// happens to model.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Element {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_view_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_phase_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_option_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation_meters: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_parameters: Vec<Parameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

/// Classes that carry a building element of the model. Kept in step with the
/// exporter's own list, which was established by joining the decode to the IFC
/// Revit exported from the same model.
const BUILDING_CLASSES: &[&str] = &[
    "SWall",
    "Floor",
    "StairsLanding",
    "StairsRun",
    "StairsElement",
    "ProfileRoof",
    "FamilyInstance",
];

impl Element {
    /// A model element, as against a type definition, an annotation or a view
    /// artefact of the same class. The three clauses are the exporter's, and
    /// keep every product Revit exports while dropping the records that are
    /// not model elements.
    #[must_use]
    pub fn is_model_element(&self) -> bool {
        self.class
            .as_deref()
            .is_some_and(|class| BUILDING_CLASSES.contains(&class))
            && self.owner_view_id.is_none()
            && self.created_phase_id.is_some()
            && self.category_name.is_none()
    }

    #[must_use]
    pub fn is_room(&self) -> bool {
        self.class.as_deref() == Some("RoomElem")
    }

    #[must_use]
    pub fn is_level(&self) -> bool {
        self.class.as_deref() == Some("Level")
    }

    fn parameter(&self, name: &str) -> Option<&Parameter> {
        self.parameters
            .iter()
            .chain(&self.type_parameters)
            .find(|parameter| parameter.name == name)
    }

    /// Lower-cased text this element can be found by.
    fn search_haystack(&self) -> String {
        let mut haystack = String::new();
        for field in [
            self.name.as_deref(),
            self.class.as_deref(),
            self.category_name.as_deref(),
            self.level_name.as_deref(),
            self.type_name.as_deref(),
            self.family_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            haystack.push_str(field);
            haystack.push('\n');
        }
        for parameter in self.parameters.iter().chain(&self.type_parameters) {
            if let Some(text) = parameter.as_search_text() {
                haystack.push_str(&text);
                haystack.push('\n');
            }
        }
        haystack.to_lowercase()
    }
}

/// One element flattened for a retrieval index: a prose summary to embed, and
/// the fields worth filtering on kept beside it.
#[derive(Debug, Serialize)]
pub struct Document {
    pub model: String,
    pub id: u32,
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_number: Option<String>,
    pub source: Option<Source>,
}

/// One loaded model.
pub struct Model {
    pub name: String,
    pub path: PathBuf,
    elements: Vec<Element>,
    by_id: BTreeMap<u32, usize>,
    haystacks: Vec<String>,
}

/// What a filtered query asks for. Every field is optional and they combine
/// with AND.
#[derive(Debug, Default)]
pub struct Query {
    pub class: Option<String>,
    pub category: Option<String>,
    pub level: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub model_elements_only: bool,
    pub with_geometry: bool,
    pub offset: usize,
    pub limit: usize,
}

impl Model {
    /// Read one `export-json` artefact. A line that does not parse is skipped
    /// and counted rather than failing the load, because a partial artefact is
    /// still worth serving and the count says how partial it is.
    pub fn load(name: &str, path: &Path) -> io::Result<(Self, usize)> {
        let reader = BufReader::with_capacity(1 << 20, File::open(path)?);
        let mut elements = Vec::new();
        let mut skipped = 0_usize;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Element>(&line) {
                Ok(element) => elements.push(element),
                Err(_) => skipped += 1,
            }
        }
        elements.shrink_to_fit();
        let by_id = elements
            .iter()
            .enumerate()
            .map(|(index, element)| (element.id, index))
            .collect();
        let haystacks = elements
            .iter()
            .map(Element::search_haystack)
            .collect::<Vec<_>>();
        Ok((
            Self {
                name: name.to_owned(),
                path: path.to_path_buf(),
                elements,
                by_id,
                haystacks,
            },
            skipped,
        ))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&Element> {
        self.by_id.get(&id).map(|index| &self.elements[*index])
    }

    fn matches(&self, index: usize, query: &Query) -> bool {
        let element = &self.elements[index];
        let equal = |field: Option<&str>, wanted: &Option<String>| {
            wanted
                .as_ref()
                .is_none_or(|wanted| field.is_some_and(|field| field.eq_ignore_ascii_case(wanted)))
        };
        let contains = |field: Option<&str>, wanted: &Option<String>| {
            wanted.as_ref().is_none_or(|wanted| {
                field.is_some_and(|field| field.to_lowercase().contains(&wanted.to_lowercase()))
            })
        };
        equal(element.class.as_deref(), &query.class)
            && equal(element.category_name.as_deref(), &query.category)
            && equal(element.level_name.as_deref(), &query.level)
            && contains(element.name.as_deref(), &query.name)
            && query
                .text
                .as_ref()
                .is_none_or(|text| self.haystacks[index].contains(&text.to_lowercase()))
            && (!query.model_elements_only || element.is_model_element())
            && (!query.with_geometry || element.geometry.is_some())
    }

    /// Elements matching `query`, and how many matched in total.
    #[must_use]
    pub fn query(&self, query: &Query) -> (Vec<&Element>, usize) {
        let mut total = 0_usize;
        let mut page = Vec::new();
        for index in 0..self.elements.len() {
            if !self.matches(index, query) {
                continue;
            }
            total += 1;
            if total > query.offset && page.len() < query.limit {
                page.push(&self.elements[index]);
            }
        }
        (page, total)
    }

    #[must_use]
    pub fn rooms(&self) -> Vec<&Element> {
        self.elements.iter().filter(|e| e.is_room()).collect()
    }

    /// The storeys of this model, by the exporter's rule so that the API and
    /// the IFC agree on what a storey is.
    ///
    /// The record walk recovers every `Level` the file mentions, including
    /// those a linked model contributes - 721 of them on AR S1 for 15 real
    /// storeys, the same name repeated at two elevations. A level is a storey
    /// only if a model element stands on it, and two levels sharing a name and
    /// an elevation are one storey however often the file repeats them.
    #[must_use]
    pub fn levels(&self) -> Vec<&Element> {
        let occupied = self
            .elements
            .iter()
            .filter(|element| element.is_model_element())
            .filter_map(|element| element.level_id)
            .collect::<std::collections::BTreeSet<_>>();
        let mut seen = std::collections::BTreeSet::new();
        let mut levels = self
            .elements
            .iter()
            .filter(|element| element.is_level() && element.elevation_meters.is_some())
            .filter(|element| occupied.contains(&i64::from(element.id)))
            .filter(|element| {
                seen.insert((
                    element.name.clone(),
                    element.elevation_meters.unwrap_or_default().to_bits(),
                ))
            })
            .collect::<Vec<_>>();
        levels.sort_by(|a, b| {
            a.elevation_meters
                .unwrap_or_default()
                .total_cmp(&b.elevation_meters.unwrap_or_default())
        });
        levels
    }

    /// Counts an agent needs before it knows what to ask for.
    #[must_use]
    pub fn summary(&self) -> serde_json::Value {
        let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
        let mut categories: BTreeMap<&str, usize> = BTreeMap::new();
        let (mut model_elements, mut with_geometry, mut with_parameters) = (0, 0, 0);
        for element in &self.elements {
            if let Some(class) = element.class.as_deref() {
                *classes.entry(class).or_default() += 1;
            }
            if let Some(category) = element.category_name.as_deref() {
                *categories.entry(category).or_default() += 1;
            }
            model_elements += usize::from(element.is_model_element());
            with_geometry += usize::from(element.geometry.is_some());
            with_parameters += usize::from(!element.parameters.is_empty());
        }
        let top = |counts: BTreeMap<&str, usize>| {
            let mut counts = counts.into_iter().collect::<Vec<_>>();
            counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            counts
                .into_iter()
                .take(40)
                .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
                .collect::<Vec<_>>()
        };
        serde_json::json!({
            "model": self.name,
            "source": self.path.to_string_lossy(),
            "elements": self.elements.len(),
            "model_elements": model_elements,
            "with_geometry": with_geometry,
            "with_parameters": with_parameters,
            "rooms": self.elements.iter().filter(|e| e.is_room()).count(),
            "levels": self.levels().len(),
            "classes": top(classes),
            "categories": top(categories),
        })
    }

    /// Flatten one element into a document for a retrieval index. Returns
    /// `None` for a record that is not worth indexing - annotation, styles,
    /// view artefacts and the rest of the record walk's output.
    #[must_use]
    pub fn document(&self, element: &Element) -> Option<Document> {
        let kind = if element.is_room() {
            "room"
        } else if element.is_level() {
            "level"
        } else if element.is_model_element() {
            "element"
        } else {
            return None;
        };

        let mut text = String::new();
        let mut line = |label: &str, value: &str| {
            if !value.is_empty() {
                text.push_str(label);
                text.push_str(": ");
                text.push_str(value);
                text.push('\n');
            }
        };
        line("Model", &self.name);
        line("Kind", kind);
        if let Some(name) = &element.name {
            line("Name", name);
        }
        if let Some(class) = &element.class {
            line("Revit class", class);
        }
        if let Some(category) = &element.category_name {
            line("Category", category);
        }
        if let Some(level) = &element.level_name {
            line("Level", level);
        }
        if let Some(type_name) = &element.type_name {
            line("Type", type_name);
        }
        if let Some(family) = &element.family_name {
            line("Family", family);
        }
        for parameter in element.parameters.iter().chain(&element.type_parameters) {
            // Only values that read as text are put in the prose. A number
            // whose unit is unknown would be a misleading thing to embed, and
            // most built-in numeric parameters have no recovered unit.
            if let Some(value) = parameter.as_search_text().filter(|v| !v.is_empty()) {
                line(&parameter.name, &value);
            } else if let (Some(storage), Some(unit)) =
                (parameter.storage_value, parameter.unit_name.as_deref())
            {
                line(&parameter.name, &format!("{storage} {unit}"));
            }
        }

        Some(Document {
            model: self.name.clone(),
            id: element.id,
            kind,
            text,
            class: element.class.clone(),
            category: element.category_name.clone(),
            level: element.level_name.clone(),
            name: element.name.clone(),
            room_number: element
                .is_room()
                .then(|| element.parameter("Number").and_then(|p| p.text.clone()))
                .flatten(),
            source: element.source.clone(),
        })
    }

    /// Every record worth putting in a retrieval index. Levels are narrowed to
    /// the storeys `levels()` reports, so the index does not carry 721 copies
    /// of 15 storeys and disagree with every other route about what exists.
    pub fn documents(&self) -> impl Iterator<Item = Document> + '_ {
        let storeys = self
            .levels()
            .iter()
            .map(|level| level.id)
            .collect::<std::collections::BTreeSet<_>>();
        self.elements
            .iter()
            .filter(move |element| !element.is_level() || storeys.contains(&element.id))
            .filter_map(|element| self.document(element))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(line: &str) -> Element {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn reads_an_export_json_line_without_losing_its_resolved_names() {
        let element = element(
            r#"{"id":4975002,"class":"SWall","level_id":3195357,"level_name":"-01 Подвал",
                "created_phase_id":3,"records":3,
                "parameters":[{"id":-1001105,"name":"Unconnected Height",
                               "built_in":"WALL_USER_HEIGHT_PARAM","double":13.12}],
                "source":{"partition":"Partitions/541","member":1606,"offset":83900}}"#,
        );
        assert_eq!(element.id, 4_975_002);
        assert_eq!(element.level_name.as_deref(), Some("-01 Подвал"));
        assert_eq!(element.parameters[0].double, Some(13.12));
        assert_eq!(element.source.as_ref().unwrap().member, 1606);
        // The three clauses the exporter uses, applied here too.
        assert!(element.is_model_element());
    }

    #[test]
    fn refuses_to_call_a_type_or_a_view_record_a_model_element() {
        // Declares its own category, so it is a type definition.
        assert!(
            !element(
                r#"{"id":1,"class":"SWall","created_phase_id":3,"category_name":"OST_Walls"}"#
            )
            .is_model_element()
        );
        // Owned by a view, so annotation.
        assert!(
            !element(r#"{"id":2,"class":"FamilyInstance","created_phase_id":3,"owner_view_id":9}"#)
                .is_model_element()
        );
        // Carries no phase.
        assert!(!element(r#"{"id":3,"class":"SWall"}"#).is_model_element());
        // Not a building class at all.
        assert!(!element(r#"{"id":4,"class":"TextNote","created_phase_id":3}"#).is_model_element());
    }

    fn model(test: &str) -> Model {
        let lines = [
            r#"{"id":1,"class":"SWall","created_phase_id":3,"level_id":5,"level_name":"01 Этаж"}"#,
            r#"{"id":2,"class":"SWall","created_phase_id":3,"level_id":5,"level_name":"02 Этаж"}"#,
            r#"{"id":6,"class":"Level","name":"01 Этаж","elevation_meters":0.0}"#,
            r#"{"id":3,"class":"RoomElem","name":"Комната","level_name":"01 Этаж","parameters":[{"id":-1006900,"name":"Number","text":"204"}]}"#,
            r#"{"id":4,"class":"TextNote","name":"примечание"}"#,
            r#"{"id":5,"class":"Level","name":"01 Этаж","elevation_meters":0.0}"#,
        ];
        // Each test gets its own file: the suite runs them in parallel and a
        // shared path lets one test read another's half-written artefact.
        let directory = std::env::temp_dir().join(format!("rivet-api-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{test}.jsonl"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (model, skipped) = Model::load(test, &path).unwrap();
        assert_eq!(skipped, 0);
        model
    }

    #[test]
    fn filters_combine_with_and_and_report_the_full_total() {
        let model = model("filters");
        let query = Query {
            class: Some("swall".to_owned()),
            limit: 1,
            ..Query::default()
        };
        let (page, total) = model.query(&query);
        // The class match is case-insensitive, the page is capped, and the
        // total counts every match rather than the page.
        assert_eq!(total, 2);
        assert_eq!(page.len(), 1);

        let query = Query {
            class: Some("SWall".to_owned()),
            level: Some("02 Этаж".to_owned()),
            limit: 10,
            ..Query::default()
        };
        assert_eq!(model.query(&query).1, 1);
    }

    #[test]
    fn free_text_search_reaches_a_parameter_value() {
        let model = model("search");
        let query = Query {
            text: Some("204".to_owned()),
            limit: 10,
            ..Query::default()
        };
        let (page, total) = model.query(&query);
        assert_eq!(total, 1);
        assert_eq!(page[0].id, 3);
    }

    #[test]
    fn indexes_only_the_records_worth_retrieving() {
        let model = model("documents");
        let documents = model.documents().collect::<Vec<_>>();
        // Two walls, one room and one level; the text note is not a product,
        // a type or a place and is left out.
        let mut kinds = documents.iter().map(|d| d.kind).collect::<Vec<_>>();
        kinds.sort_unstable();
        // One level, not two: the index carries the storeys, not every Level
        // record the file mentions.
        assert_eq!(kinds, ["element", "element", "level", "room"]);
        let room = documents.iter().find(|d| d.kind == "room").unwrap();
        assert_eq!(room.room_number.as_deref(), Some("204"));
        assert!(room.text.contains("Комната"), "{}", room.text);
        assert!(room.text.contains("Level: 01 Этаж"), "{}", room.text);
    }

    #[test]
    fn a_storey_is_a_level_a_model_element_stands_on_counted_once() {
        // Level 5 is stood on by both walls; level 6 repeats its name and
        // elevation but nothing stands on it, and it is not a second storey.
        let model = model("storeys");
        let levels = model.levels();
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].id, 5);
    }

    #[test]
    fn summary_counts_what_an_agent_needs_before_asking() {
        let summary = model("summary").summary();
        assert_eq!(summary["elements"], 6);
        assert_eq!(summary["model_elements"], 2);
        assert_eq!(summary["rooms"], 1);
        assert_eq!(summary["levels"], 1);
    }
}

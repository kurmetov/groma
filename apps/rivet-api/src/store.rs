//! The queryable side of a model: load one `export-json` artefact, keep it in
//! memory, and answer the questions an agent asks of it.
//!
//! The store deliberately does no decoding of its own. It reads what
//! `rivet export-json` produced, so anything it reports is traceable to that
//! artefact and to the `source` record inside it, and a decode improvement
//! reaches the API by re-running the export rather than by changing this.

use std::collections::{BTreeMap, BTreeSet};
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
    /// Storey name. Names repeat - AR S1 has `02 Этаж` at two elevations - so
    /// this merges every storey that carries the name. Use `level_ids` from
    /// `storey_level_ids` to name one storey exactly.
    pub level: Option<String>,
    /// Every `Level` record of one storey, from `storey_level_ids`. An element
    /// names one record as its level and a storey is many records, so a storey
    /// filter is a set rather than an id.
    pub level_ids: Option<BTreeSet<i64>>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub model_elements_only: bool,
    pub with_geometry: bool,
    pub offset: usize,
    pub limit: usize,
}

/// A page of matches, with what the filters removed alongside it. An agent
/// that asked for rooms and model elements at once would otherwise read the
/// resulting `0` as "this model has no rooms".
pub struct Page<'a> {
    pub elements: Vec<&'a Element>,
    pub total: usize,
    /// Matched every other filter and was dropped only by
    /// `model_elements_only`, which no room, level or type definition passes.
    pub excluded_as_not_model_elements: usize,
}

/// One storey: every `Level` record sharing a name and an elevation, folded
/// into the single place they all describe.
#[derive(Debug, Serialize)]
pub struct Storey {
    /// Lowest of the folded record ids, and the value a storey filter takes.
    pub id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub elevation_meters: f64,
    /// Every `Level` record folded into this storey. Elements are spread over
    /// all of them: on AR S1, 15 storeys are named by 152 records, and reading
    /// only the first of each loses 3 587 of the 15 149 placed elements.
    pub level_ids: Vec<u32>,
    /// Another storey carries this name at a different elevation, so the
    /// `level` name filter cannot separate the two.
    pub ambiguous_name: bool,
    /// Model elements standing on any of `level_ids`.
    pub model_elements: usize,
    /// Rooms on any of `level_ids`.
    pub rooms: usize,
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
            // `export-json --full` opens the artefact with a model line
            // indexing the file. It is not an element and its absence is not
            // partiality, so it is passed over without counting.
            if line.starts_with("{\"kind\":\"model\"") {
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
            && query.level_ids.as_ref().is_none_or(|ids| {
                element
                    .level_id
                    .is_some_and(|level_id| ids.contains(&level_id))
            })
            && contains(element.name.as_deref(), &query.name)
            && query
                .text
                .as_ref()
                .is_none_or(|text| self.haystacks[index].contains(&text.to_lowercase()))
            && (!query.with_geometry || element.geometry.is_some())
    }

    /// Elements matching `query`, how many matched in total, and how many the
    /// `model_elements_only` flag alone removed.
    #[must_use]
    pub fn query(&self, query: &Query) -> Page<'_> {
        let mut total = 0_usize;
        let mut excluded_as_not_model_elements = 0_usize;
        let mut elements = Vec::new();
        for index in 0..self.elements.len() {
            if !self.matches(index, query) {
                continue;
            }
            // Applied here rather than in `matches` so the answer can say what
            // it cost: a room passes every other filter and fails this one.
            if query.model_elements_only && !self.elements[index].is_model_element() {
                excluded_as_not_model_elements += 1;
                continue;
            }
            total += 1;
            if total > query.offset && elements.len() < query.limit {
                elements.push(&self.elements[index]);
            }
        }
        Page {
            elements,
            total,
            excluded_as_not_model_elements,
        }
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
    /// only if a model element stands on it, and every level sharing a name
    /// and an elevation is the same storey however often the file repeats it.
    ///
    /// The repeats are not empty duplicates: 152 records carry the 15 storeys
    /// of AR S1 and elements are spread across all of them, so a storey folds
    /// them together and counts over the whole set.
    #[must_use]
    pub fn storeys(&self) -> Vec<Storey> {
        // A storey is keyed by what makes it one place. `to_bits` is the exact
        // comparison an elevation deserves: two records either carry the same
        // double or describe different heights.
        let mut folded: BTreeMap<(Option<&str>, u64), Vec<&Element>> = BTreeMap::new();
        for element in &self.elements {
            if !element.is_level() {
                continue;
            }
            let Some(elevation) = element.elevation_meters else {
                continue;
            };
            folded
                .entry((element.name.as_deref(), elevation.to_bits()))
                .or_default()
                .push(element);
        }

        let (mut model_elements, mut rooms) = (BTreeMap::new(), BTreeMap::new());
        for element in &self.elements {
            let Some(level_id) = element.level_id else {
                continue;
            };
            if element.is_model_element() {
                *model_elements.entry(level_id).or_insert(0_usize) += 1;
            } else if element.is_room() {
                *rooms.entry(level_id).or_insert(0_usize) += 1;
            }
        }

        let mut names: BTreeMap<Option<&str>, usize> = BTreeMap::new();
        let mut storeys = Vec::new();
        for ((name, elevation), records) in folded {
            let mut level_ids = records.iter().map(|level| level.id).collect::<Vec<_>>();
            level_ids.sort_unstable();
            let count = |counts: &BTreeMap<i64, usize>| {
                level_ids
                    .iter()
                    .filter_map(|id| counts.get(&i64::from(*id)))
                    .sum::<usize>()
            };
            let standing = count(&model_elements);
            let rooms = count(&rooms);
            // The exporter's rule: a level nothing stands on is not a storey.
            if standing == 0 {
                continue;
            }
            *names.entry(name).or_default() += 1;
            storeys.push(Storey {
                id: level_ids[0],
                name: name.map(str::to_owned),
                elevation_meters: f64::from_bits(elevation),
                level_ids,
                ambiguous_name: false,
                model_elements: standing,
                rooms,
            });
        }
        for storey in &mut storeys {
            storey.ambiguous_name = names
                .get(&storey.name.as_deref())
                .is_some_and(|count| *count > 1);
        }
        storeys.sort_by(|a, b| a.elevation_meters.total_cmp(&b.elevation_meters));
        storeys
    }

    /// Every `Level` record of the storey `id` names, for `Query::level_ids`.
    /// `None` when no storey has that id, so a caller can refuse the filter
    /// rather than answer it with an empty page.
    #[must_use]
    pub fn storey_level_ids(&self, id: u32) -> Option<BTreeSet<i64>> {
        self.storeys()
            .into_iter()
            .find(|storey| storey.id == id)
            .map(|storey| storey.level_ids.iter().map(|id| i64::from(*id)).collect())
    }

    /// Counts an agent needs before it knows what to ask for.
    #[must_use]
    pub fn summary(&self) -> serde_json::Value {
        let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
        let mut categories: BTreeMap<&str, usize> = BTreeMap::new();
        let (mut model_elements, mut with_geometry, mut with_parameters) = (0, 0, 0);
        let mut model_elements_without_level = 0;
        for element in &self.elements {
            if let Some(class) = element.class.as_deref() {
                *classes.entry(class).or_default() += 1;
            }
            if let Some(category) = element.category_name.as_deref() {
                *categories.entry(category).or_default() += 1;
            }
            model_elements += usize::from(element.is_model_element());
            model_elements_without_level +=
                usize::from(element.is_model_element() && element.level_id.is_none());
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
            // Placed on no level at all, so no per-storey count reaches them
            // and the storey counts do not sum to `model_elements`.
            "model_elements_without_level": model_elements_without_level,
            "levels": self.storeys().len(),
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
            .storeys()
            .iter()
            .map(|storey| storey.id)
            .collect::<BTreeSet<_>>();
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
        // Two storeys sharing a name, as AR S1 has: one at 0.0 written as two
        // `Level` records with an element on each, one at 3.3, plus a level
        // nothing stands on and a wall standing on no level.
        let lines = [
            r#"{"id":1,"class":"SWall","created_phase_id":3,"level_id":5,"level_name":"01 Этаж"}"#,
            r#"{"id":2,"class":"SWall","created_phase_id":3,"level_id":6,"level_name":"01 Этаж"}"#,
            r#"{"id":6,"class":"Level","name":"01 Этаж","elevation_meters":0.0}"#,
            r#"{"id":3,"class":"RoomElem","name":"Комната","level_id":5,"level_name":"01 Этаж","parameters":[{"id":-1006900,"name":"Number","text":"204"}]}"#,
            r#"{"id":4,"class":"TextNote","name":"примечание"}"#,
            r#"{"id":5,"class":"Level","name":"01 Этаж","elevation_meters":0.0}"#,
            r#"{"id":7,"class":"Level","name":"01 Этаж","elevation_meters":3.3}"#,
            r#"{"id":8,"class":"SWall","created_phase_id":3,"level_id":7,"level_name":"01 Этаж"}"#,
            r#"{"id":9,"class":"SWall","created_phase_id":3}"#,
            r#"{"id":10,"class":"Level","name":"02 Этаж","elevation_meters":6.6}"#,
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
        let page = model.query(&query);
        // The class match is case-insensitive, the page is capped, and the
        // total counts every match rather than the page.
        assert_eq!(page.total, 4);
        assert_eq!(page.elements.len(), 1);

        // A name reaches both storeys that carry it, which is why it is not
        // enough on its own.
        let query = Query {
            class: Some("SWall".to_owned()),
            level: Some("01 Этаж".to_owned()),
            limit: 10,
            ..Query::default()
        };
        assert_eq!(model.query(&query).total, 3);
    }

    #[test]
    fn a_storey_filter_separates_two_storeys_that_share_a_name() {
        let model = model("storey-filter");
        let ground = model.storey_level_ids(5).unwrap();
        let upper = model.storey_level_ids(7).unwrap();
        assert_eq!(ground, BTreeSet::from([5, 6]));
        assert_eq!(upper, BTreeSet::from([7]));
        let walls = |level_ids| {
            model
                .query(&Query {
                    class: Some("SWall".to_owned()),
                    level_ids: Some(level_ids),
                    limit: 10,
                    ..Query::default()
                })
                .total
        };
        // Both records of the lower storey are counted, and the upper storey
        // of the same name is not.
        assert_eq!(walls(ground), 2);
        assert_eq!(walls(upper), 1);
        // An id that is not a storey is refused rather than answered with 0.
        assert!(model.storey_level_ids(10).is_none());
    }

    #[test]
    fn reports_what_the_model_elements_flag_removed() {
        let model = model("excluded");
        // The rooms are there, and this flag is what hides them - an answer
        // that said only `0` would read as "this model has no rooms".
        let page = model.query(&Query {
            class: Some("RoomElem".to_owned()),
            model_elements_only: true,
            limit: 10,
            ..Query::default()
        });
        assert_eq!(page.total, 0);
        assert_eq!(page.excluded_as_not_model_elements, 1);
        let page = model.query(&Query {
            class: Some("RoomElem".to_owned()),
            limit: 10,
            ..Query::default()
        });
        assert_eq!(page.total, 1);
        assert_eq!(page.excluded_as_not_model_elements, 0);
    }

    #[test]
    fn free_text_search_reaches_a_parameter_value() {
        let model = model("search");
        let query = Query {
            text: Some("204".to_owned()),
            limit: 10,
            ..Query::default()
        };
        let page = model.query(&query);
        assert_eq!(page.total, 1);
        assert_eq!(page.elements[0].id, 3);
    }

    #[test]
    fn indexes_only_the_records_worth_retrieving() {
        let model = model("documents");
        let documents = model.documents().collect::<Vec<_>>();
        // Two walls, one room and one level; the text note is not a product,
        // a type or a place and is left out.
        let mut kinds = documents.iter().map(|d| d.kind).collect::<Vec<_>>();
        kinds.sort_unstable();
        // Two levels for two storeys, not the four Level records the file
        // holds: the index carries the storeys.
        assert_eq!(
            kinds,
            [
                "element", "element", "element", "element", "level", "level", "room"
            ]
        );
        let room = documents.iter().find(|d| d.kind == "room").unwrap();
        assert_eq!(room.room_number.as_deref(), Some("204"));
        assert!(room.text.contains("Комната"), "{}", room.text);
        assert!(room.text.contains("Level: 01 Этаж"), "{}", room.text);
    }

    #[test]
    fn a_storey_folds_every_level_record_that_describes_it() {
        let model = model("storeys");
        let storeys = model.storeys();
        // Two storeys, lowest first. Level 10 is stood on by nothing and is
        // not one of them.
        assert_eq!(storeys.len(), 2);
        assert_eq!(storeys[0].id, 5);
        assert_eq!(storeys[0].level_ids, [5, 6]);
        assert_eq!(storeys[0].elevation_meters.to_bits(), 0.0_f64.to_bits());
        // Both records of the storey are counted, not just the first.
        assert_eq!(storeys[0].model_elements, 2);
        assert_eq!(storeys[0].rooms, 1);
        assert_eq!(storeys[1].id, 7);
        assert_eq!(storeys[1].model_elements, 1);
        assert_eq!(storeys[1].rooms, 0);
        // Both are called `01 Этаж`, so the name filter cannot tell them
        // apart and each says so.
        assert!(storeys.iter().all(|storey| storey.ambiguous_name));
    }

    #[test]
    fn summary_counts_what_an_agent_needs_before_asking() {
        let summary = model("summary").summary();
        assert_eq!(summary["elements"], 10);
        assert_eq!(summary["model_elements"], 4);
        assert_eq!(summary["rooms"], 1);
        assert_eq!(summary["levels"], 2);
        // The wall on no level: the storey counts sum to 3, not to 4.
        assert_eq!(summary["model_elements_without_level"], 1);
    }
}

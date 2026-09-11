use rvt_schema::{FieldType, Schema};

use crate::member::RecordString;
use crate::serial::{SerialObject, walk_record_collecting_classes};

/// Largest parameter count accepted from one stored set.
pub const MAX_PARAMETERS_PER_SET: u32 = 4096;
/// Range a built-in parameter code falls into. Built-in codes are negative;
/// anything else must be the identifier of a parameter element.
///
/// Measured over 163,260 recovered values: 99.6% land in `-1_200_000..-1_152`,
/// and the remaining 0.4% scatter down to -16M, which is what a chance match
/// looks like. The window is kept a little wider than the observed cluster and
/// far narrower than the `i32` range, so a stray word is unlikely to pass.
const BUILT_IN_PARAMETER_RANGE: std::ops::Range<i32> = -2_000_000..-1_000;

/// Prefix of measurable and non-measurable Forge spec identifiers.
pub const AUTODESK_SPEC_PREFIX: &str = "autodesk.spec.";

/// The Forge spec carried by a project/shared parameter definition.
///
/// A stored parameter value points to its definition by positive element ID.
/// The definition's `ParamDef` object in turn carries this type ID. The spec,
/// rather than a document display unit, determines the value's dimension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParameterSpec {
    pub offset: usize,
    pub type_id: String,
}

impl ParameterSpec {
    /// Find the single Forge spec identifier in a parameter-definition body.
    ///
    /// The string encoding is deterministic, but its owning `ParamDef`
    /// subclass is variable, so this remains a bounded scan. A body with more
    /// than one spec identifier is rejected rather than choosing one.
    #[must_use]
    pub fn scan(body: &[u8]) -> Option<Self> {
        let mut found = None;
        for offset in 0..body.len().saturating_sub(4) {
            let Some(value) = RecordString::parse_at_lenient(body, offset) else {
                continue;
            };
            if !value.value.starts_with(AUTODESK_SPEC_PREFIX) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(Self {
                offset,
                type_id: value.value,
            });
        }
        found
    }
}

/// A parameter value as stored, without unit conversion or interpretation.
#[derive(Clone, Debug, PartialEq)]
pub enum ParameterValue {
    Double(f64),
    Integer(i32),
    Text(String),
    /// An `ElementId` value: a reference to another element.
    Reference(i32),
}

/// One stored parameter: its identifier and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    /// Negative for a built-in parameter code, positive for a parameter
    /// element that carries the parameter's name.
    pub id: i32,
    pub value: ParameterValue,
}

impl Parameter {
    #[must_use]
    pub const fn is_built_in(&self) -> bool {
        self.id < 0
    }
}

/// The classes behind the parameters a loadable family stores on its records.
///
/// `FamilyParams.m_params` is a counted collection of `NamedParam`, and a
/// `NamedParam` is 30 bytes: a name, an expression reference, a `Float64`, two
/// `ElementId`s and an `Integer32`, then two flags. The values a walk collects
/// for one `FamilyParams` object are therefore N strings, N numbers, 3N
/// integers and 2N small integers, and that shape is the read's own check -
/// on AR S1's `FamilySymbol` records 5 059 of 5 062 objects pair exactly, and
/// the three that do not are dropped rather than guessed at.
///
/// This is where a loadable family keeps what the four `ParamValueSet` objects
/// keep for a system family. Before this was read, AR S1 yielded about 1 200
/// values from every `FamilySymbol` in the file - seven distinct built-in
/// codes - while the same records hold 118 087 `NamedParam` entries and its
/// `FamilyInstance` records hold a further 126 605.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FamilyParameterClassIndexes {
    pub params: u16,
    pub named: u16,
}

/// What a `NamedParam` declares, in order. The read pairs values by position,
/// so a schema that declares anything else is not decoded at all.
const NAMED_PARAMETER_PROPERTIES: &[(&str, FieldType)] = &[
    ("m_str", FieldType::String),
    ("m_oExpression", FieldType::Object),
    ("m_value", FieldType::Float64),
    ("m_elemId", FieldType::Object),
    ("m_paramId", FieldType::Object),
    ("m_int", FieldType::Integer32),
    ("m_instance", FieldType::Bool),
    ("m_reporting", FieldType::Bool),
];

impl FamilyParameterClassIndexes {
    /// Resolve the two classes and verify they declare what this module reads.
    ///
    /// `None` when either class is missing, when `FamilyParams.m_params` is not
    /// a collection naming `NamedParam` as its element class, or when
    /// `NamedParam` declares anything other than [`NAMED_PARAMETER_PROPERTIES`]
    /// in that order. A schema that changed the layout is thereby not decoded
    /// at all, rather than decoded wrongly.
    #[must_use]
    pub fn detect(schema: &Schema) -> Option<Self> {
        let params = schema.class_by_name("FamilyParams")?;
        let named = schema.class_by_name("NamedParam")?;
        if named.parent != rvt_schema::TypeReference::None
            || named.properties.len() != NAMED_PARAMETER_PROPERTIES.len()
            || !named
                .properties
                .iter()
                .zip(NAMED_PARAMETER_PROPERTIES)
                .all(|(declared, (name, field_type))| {
                    declared.name == *name && declared.field_type == *field_type
                })
        {
            return None;
        }
        let collection = params.properties.first()?;
        if collection.name != "m_params"
            || collection.item_mode != 5
            || collection.loading_mode != 0
            || collection.static_type.as_ref()?.index()? != named.index
        {
            return None;
        }
        Some(Self {
            params: params.index,
            named: named.index,
        })
    }
}

/// The parameters one walked `FamilyParams` object holds.
///
/// A `NamedParam` carries three value slots - a `Float64`, an `ElementId` and
/// an `Integer32` - and exactly one of them is ever filled: over AR S1's
/// 118 087 entries, 42 633 carry the double, 15 131 the reference, 10 888 the
/// integer and **not one carries two**. So the filled slot names the value's
/// kind, and no display unit or spec has to be consulted to choose it. An
/// entry with no slot filled states a zero whose kind nothing establishes, and
/// is left out rather than written as one kind or the other.
///
/// `m_str` is empty on every entry the corpus holds - a family parameter is
/// named by the element `m_paramId` points at, which is where
/// `parameter_names` reads it - and `m_instance` reads zero on every entry of
/// every `FamilyInstance` record, so neither is interpreted here.
#[must_use]
pub fn read_family_parameters(
    object: &SerialObject,
    classes: FamilyParameterClassIndexes,
) -> Vec<Parameter> {
    if object.class_index != classes.params {
        return Vec::new();
    }
    let count = object.numbers.len();
    if count == 0
        || object.strings.len() != count
        || object.integers.len() != count.saturating_mul(3)
        || object.small_integers.len() != count.saturating_mul(2)
    {
        return Vec::new();
    }
    let mut parameters = Vec::new();
    for at in 0..count {
        let element_id = object.integers[at * 3];
        let parameter_id = object.integers[at * 3 + 1];
        let integer = object.integers[at * 3 + 2];
        let double = object.numbers[at];
        let value = match (double != 0.0, integer != 0, element_id > 0) {
            (true, false, false) => ParameterValue::Double(double),
            (false, true, false) => ParameterValue::Integer(integer),
            (false, false, true) => ParameterValue::Reference(element_id),
            // No slot filled, or more than one: nothing establishes which kind
            // the value is, so none is written.
            _ => continue,
        };
        parameters.push(Parameter {
            id: parameter_id,
            value,
        });
    }
    parameters
}

/// Read both kinds of stored parameter out of one record in one walk: the four
/// typed `ParamValueSet` objects a system family uses, and the `FamilyParams`
/// collection a loadable family uses. See [`ParameterSets::from_record`] and
/// [`read_family_parameters`].
///
/// One walk rather than two because a record's node stream is walked in full
/// either way, and on the corpus that stream is mostly boundary geometry: the
/// second pass would cost as much as the first and find the same objects.
#[must_use]
pub fn read_record_parameters(
    schema: &Schema,
    class_index: u16,
    body: &[u8],
    sets: ParameterSetClassIndexes,
    family: Option<FamilyParameterClassIndexes>,
) -> (Option<ParameterSets>, Vec<Parameter>) {
    let mut keep = vec![sets.double, sets.integer, sets.text, sets.reference];
    if let Some(family) = family {
        keep.push(family.params);
    }
    let (_walk, objects) = walk_record_collecting_classes(schema, class_index, body, &keep);
    let family_parameters = family.map_or_else(Vec::new, |family| {
        objects
            .iter()
            .flat_map(|object| read_family_parameters(object, family))
            .collect()
    });
    (
        ParameterSets::from_objects(&objects, sets),
        family_parameters,
    )
}

/// The four typed parameter sets an element stores, in schema order.
///
/// `Element` declares `m_pParamValueSetDouble`, `m_pParamValueSetInt`,
/// `m_pParamValueSetAString` and `m_pParamValueSetElementId`. Each set is
/// `[count:u32]` followed by that many entries, and the sets are stored one
/// after another. The preferred reader binds the value kinds to dynamic set
/// class references before `m_id` and accepts only one matching run. The
/// older unbound scan remains available for unsupported schemas as diagnostic
/// output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParameterSets {
    /// Offset in the body where the run of sets starts.
    pub offset: usize,
    /// Bytes the run occupies.
    pub encoded_bytes: usize,
    pub parameters: Vec<Parameter>,
    /// How many of the four typed sets were present.
    pub sets: usize,
}

/// Dynamic class indexes of the four parameter-set objects declared by
/// `Element`. They are resolved from each file's `Formats/Latest` schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParameterSetClassIndexes {
    pub double: u16,
    pub integer: u16,
    pub text: u16,
    pub reference: u16,
}

impl ParameterSets {
    /// Read the parameter sets a record's `Element` declarations name, from
    /// the walked node stream rather than by scanning the body for them.
    ///
    /// `Element` declares the four typed sets as its first four properties, so
    /// the walk meets their objects at the head of the node stream, before
    /// anything a scan could confuse them with. Each set object holds one
    /// counted collection of value objects, and each value object's declared
    /// fields land in the walked object's collected values in declaration
    /// order:
    ///
    /// * `ParamValueDouble` is `m_value` then `m_paramId`, so its doubles pair
    ///   with its integers by position;
    /// * `ParamValueInt` and `ParamValueElementId` are `m_paramId` then
    ///   `m_value`, both `Integer32`, so their integers alternate;
    /// * `ParamValueAString` is `m_paramId` then `m_value`, so its integers
    ///   pair with its strings by position.
    ///
    /// Every one of those classes declares exactly those two fields and no
    /// parent, so nothing else of the same kind can land in the same vector. A
    /// set whose collected values do not pair up is dropped rather than guessed
    /// at, and identifiers are reported as stored: this reads the declarations,
    /// so there is nothing here for an allow-list to confirm.
    #[must_use]
    pub fn from_record(
        schema: &Schema,
        class_index: u16,
        body: &[u8],
        classes: ParameterSetClassIndexes,
    ) -> Option<Self> {
        // Only the four set classes are materialized. `read_value_set` answers
        // for those four and for nothing else, so the objects left out could
        // not have contributed a parameter; on a record whose node stream is a
        // solid, they are every face in it.
        let (_walk, objects) = walk_record_collecting_classes(
            schema,
            class_index,
            body,
            &[
                classes.double,
                classes.integer,
                classes.text,
                classes.reference,
            ],
        );
        Self::from_objects(&objects, classes)
    }

    /// The set-reading half of [`ParameterSets::from_record`], for a node
    /// stream that has already been walked.
    #[must_use]
    pub fn from_objects(
        objects: &[SerialObject],
        classes: ParameterSetClassIndexes,
    ) -> Option<Self> {
        let mut parameters = Vec::new();
        let mut sets = 0_usize;
        let mut first = None;
        let mut last = 0_usize;
        for object in objects {
            let Some(read) = read_value_set(object, classes) else {
                continue;
            };
            sets += 1;
            first = Some(first.map_or(object.offset, |at: usize| at.min(object.offset)));
            last = last.max(object.offset.saturating_add(object.bytes));
            parameters.extend(read);
        }
        let offset = first?;
        Some(Self {
            offset,
            encoded_bytes: last.saturating_sub(offset),
            parameters,
            sets,
        })
    }

    /// Read the one parameter run whose value kinds agree with the dynamic
    /// set classes referenced before the element's `m_id` field.
    ///
    /// `search_start` should be the end of the fixed `Element` tail and
    /// `pointer_end` the offset of `m_id`. No result is returned if there are
    /// no set-class references or if more than one matching run exists.
    #[must_use]
    pub fn scan_schema_bound(
        body: &[u8],
        search_start: usize,
        pointer_end: usize,
        classes: ParameterSetClassIndexes,
        accept_id: &impl Fn(i32) -> bool,
    ) -> Option<Self> {
        let kinds = referenced_kinds(body.get(..pointer_end)?, classes)?;
        let mut found = None;
        for offset in search_start..body.len().saturating_sub(3) {
            let Some(candidate) = Self::read_typed_run(body, offset, &kinds, accept_id) else {
                continue;
            };
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
        found
    }

    /// Scan a record body for the run of parameter sets.
    ///
    /// `accept_id` decides whether a positive identifier belongs to a known
    /// parameter element; negative built-in codes are accepted on their own.
    #[must_use]
    pub fn scan(body: &[u8], accept_id: &impl Fn(i32) -> bool) -> Option<Self> {
        let accept = |id: i32| {
            if id < 0 {
                BUILT_IN_PARAMETER_RANGE.contains(&id)
            } else {
                accept_id(id)
            }
        };
        Self::scan_accepting(body, &accept)
    }

    /// Scan using a caller-supplied allow-list for every identifier, including
    /// negative built-ins. This is preferred when a release-specific public
    /// catalog is available.
    #[must_use]
    pub fn scan_verified(body: &[u8], accept_id: &impl Fn(i32) -> bool) -> Option<Self> {
        Self::scan_accepting(body, accept_id)
    }

    fn scan_accepting(body: &[u8], accept_id: &impl Fn(i32) -> bool) -> Option<Self> {
        (0..body.len().saturating_sub(4)).find_map(|offset| Self::read_run(body, offset, accept_id))
    }

    /// Read the four sets in schema order starting at `offset`.
    fn read_run(body: &[u8], offset: usize, accept: &impl Fn(i32) -> bool) -> Option<Self> {
        let mut cursor = offset;
        let mut parameters = Vec::new();
        let mut sets = 0_usize;

        for kind in [Kind::Double, Kind::Integer, Kind::Text, Kind::Reference] {
            let Some((next, mut read)) = read_set(body, cursor, kind, accept) else {
                continue;
            };
            sets += 1;
            parameters.append(&mut read);
            cursor = next;
        }
        // One built-in-only set is strong when the caller checked every code
        // against its release catalog. Positive IDs occur throughout record
        // bodies, so a lone project/shared parameter needs independent
        // evidence: another typed set or at least three entries.
        let built_in_only = !parameters.is_empty() && parameters.iter().all(Parameter::is_built_in);
        (sets >= 2 || parameters.len() >= 3 || built_in_only).then(|| Self {
            offset,
            encoded_bytes: cursor - offset,
            parameters,
            sets,
        })
    }

    fn read_typed_run(
        body: &[u8],
        offset: usize,
        kinds: &[Kind],
        accept: &impl Fn(i32) -> bool,
    ) -> Option<Self> {
        let mut cursor = offset;
        let mut parameters = Vec::new();
        for kind in kinds {
            let (next, mut read) = read_set(body, cursor, *kind, accept)?;
            parameters.append(&mut read);
            cursor = next;
        }
        Some(Self {
            offset,
            encoded_bytes: cursor - offset,
            parameters,
            sets: kinds.len(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Double,
    Integer,
    Text,
    Reference,
}

fn referenced_kinds(prefix: &[u8], classes: ParameterSetClassIndexes) -> Option<Vec<Kind>> {
    let mut found = [
        (Kind::Double, classes.double, None),
        (Kind::Integer, classes.integer, None),
        (Kind::Text, classes.text, None),
        (Kind::Reference, classes.reference, None),
    ];
    for offset in 0..prefix.len().saturating_sub(3) {
        if prefix.get(offset..offset + 2) != Some(&[0xff, 0xff]) {
            continue;
        }
        let index = u16::from_le_bytes([prefix[offset + 2], prefix[offset + 3]]);
        for (_, wanted, position) in &mut found {
            if index == *wanted {
                if position.is_some() {
                    return None;
                }
                *position = Some(offset);
            }
        }
    }
    let mut kinds = found
        .into_iter()
        .filter_map(|(kind, _, position)| Some((position?, kind)))
        .collect::<Vec<_>>();
    if kinds.is_empty() {
        return None;
    }
    kinds.sort_unstable_by_key(|(position, _)| *position);
    Some(kinds.into_iter().map(|(_, kind)| kind).collect())
}

/// Read one `[count:u32]`-prefixed set, returning the offset after it.
fn read_set(
    body: &[u8],
    offset: usize,
    kind: Kind,
    accept: &impl Fn(i32) -> bool,
) -> Option<(usize, Vec<Parameter>)> {
    let prefix = body.get(offset..offset + 4)?;
    let count = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    if count == 0 || count > MAX_PARAMETERS_PER_SET {
        return None;
    }

    let mut cursor = offset + 4;
    let mut parameters = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (next, parameter) = read_parameter(body, cursor, kind)?;
        if !accept(parameter.id) {
            return None;
        }
        parameters.push(parameter);
        cursor = next;
    }
    Some((cursor, parameters))
}

fn read_parameter(body: &[u8], offset: usize, kind: Kind) -> Option<(usize, Parameter)> {
    let read_i32 = |at: usize| -> Option<i32> {
        let bytes = body.get(at..at + 4)?;
        Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    match kind {
        Kind::Double => {
            let bytes = body.get(offset..offset + 8)?;
            let value = f64::from_le_bytes(bytes.try_into().ok()?);
            if !value.is_finite() {
                return None;
            }
            let id = read_i32(offset + 8)?;
            Some((
                offset + 12,
                Parameter {
                    id,
                    value: ParameterValue::Double(value),
                },
            ))
        }
        Kind::Integer => {
            let id = read_i32(offset)?;
            let value = read_i32(offset + 4)?;
            Some((
                offset + 8,
                Parameter {
                    id,
                    value: ParameterValue::Integer(value),
                },
            ))
        }
        Kind::Reference => {
            let id = read_i32(offset)?;
            let value = read_i32(offset + 4)?;
            Some((
                offset + 8,
                Parameter {
                    id,
                    value: ParameterValue::Reference(value),
                },
            ))
        }
        Kind::Text => {
            let id = read_i32(offset)?;
            let text = RecordString::parse_at_lenient(body, offset + 4)?;
            Some((
                offset + 4 + text.encoded_bytes,
                Parameter {
                    id,
                    value: ParameterValue::Text(text.value),
                },
            ))
        }
    }
}

/// Read one walked parameter-set object into its stored parameters, or `None`
/// when the object is not a parameter set. A set whose collected values do not
/// pair up returns an empty list: the object is a set, but nothing in it is
/// read rather than half of it being guessed at.
fn read_value_set(
    object: &SerialObject,
    classes: ParameterSetClassIndexes,
) -> Option<Vec<Parameter>> {
    let class = object.class_index;
    if class == classes.double {
        if object.numbers.len() != object.integers.len() {
            return Some(Vec::new());
        }
        return Some(
            object
                .numbers
                .iter()
                .zip(&object.integers)
                .map(|(value, id)| Parameter {
                    id: *id,
                    value: ParameterValue::Double(*value),
                })
                .collect(),
        );
    }
    if class == classes.text {
        if object.strings.len() != object.integers.len() {
            return Some(Vec::new());
        }
        return Some(
            object
                .integers
                .iter()
                .zip(&object.strings)
                .map(|(id, value)| Parameter {
                    id: *id,
                    value: ParameterValue::Text(value.clone()),
                })
                .collect(),
        );
    }
    if class == classes.integer || class == classes.reference {
        if object.integers.len() % 2 != 0 {
            return Some(Vec::new());
        }
        let reference = class == classes.reference;
        return Some(
            object
                .integers
                .chunks_exact(2)
                .map(|pair| Parameter {
                    id: pair[0],
                    value: if reference {
                        ParameterValue::Reference(pair[1])
                    } else {
                        ParameterValue::Integer(pair[1])
                    },
                })
                .collect(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    /// The four parameter-set classes, as a walk would report them.
    fn set_classes() -> ParameterSetClassIndexes {
        ParameterSetClassIndexes {
            double: 2978,
            integer: 2980,
            text: 2977,
            reference: 2979,
        }
    }

    fn set_object(class_index: u16, offset: usize) -> SerialObject {
        SerialObject {
            object_id: 1,
            class_index,
            offset,
            bytes: 8,
            references: Vec::new(),
            identifiers: Vec::new(),
            numbers: Vec::new(),
            integers: Vec::new(),
            strings: Vec::new(),
            small_integers: Vec::new(),
            alternate_integers: Vec::new(),
        }
    }

    /// The classes a `FamilyParams` read needs, as a schema would report them.
    fn family_classes() -> FamilyParameterClassIndexes {
        FamilyParameterClassIndexes {
            params: 765,
            named: 766,
        }
    }

    /// One `FamilyParams` object holding `entries` `NamedParam`s, laid out the
    /// way a walk collects them: the values of all the entries concatenated,
    /// each vector in declaration order.
    fn family_object(entries: &[(f64, i32, i32, i32)]) -> SerialObject {
        let mut object = set_object(family_classes().params, 100);
        for (value, element_id, parameter_id, integer) in entries {
            object.strings.push(String::new());
            object.numbers.push(*value);
            object.integers.extend([*element_id, *parameter_id, *integer]);
            object.small_integers.extend([0, 0]);
        }
        object
    }

    /// Exactly one of a `NamedParam`'s three value slots is ever filled, and
    /// which one it is names the value's kind. An entry with none filled
    /// states a zero whose kind nothing establishes, so none is written.
    #[test]
    fn a_family_parameter_takes_its_kind_from_the_slot_that_is_filled() {
        let found = read_family_parameters(
            &family_object(&[
                (2.460_629_921_259_842_6, -1, 8_142_843, 0),
                (0.0, -1, 8_142_841, 1),
                (0.0, 8_142_779, 8_142_806, 0),
                (0.0, -1, 8_142_822, 0),
            ]),
            family_classes(),
        );
        assert_eq!(
            found,
            vec![
                Parameter {
                    id: 8_142_843,
                    value: ParameterValue::Double(2.460_629_921_259_842_6),
                },
                Parameter {
                    id: 8_142_841,
                    value: ParameterValue::Integer(1),
                },
                Parameter {
                    id: 8_142_806,
                    value: ParameterValue::Reference(8_142_779),
                },
            ],
            "the fourth entry fills no slot and is left out"
        );
    }

    /// The 30-byte entry shape is the read's own check: values that do not
    /// pair up are dropped rather than sliced into whatever fits.
    #[test]
    fn a_family_parameter_run_whose_values_do_not_pair_is_dropped() {
        let mut object = family_object(&[(1.0, -1, 7, 0), (2.0, -1, 8, 0)]);
        object.integers.pop();
        assert!(read_family_parameters(&object, family_classes()).is_empty());

        let mut spare = family_object(&[(1.0, -1, 7, 0)]);
        spare.small_integers.push(0);
        assert!(read_family_parameters(&spare, family_classes()).is_empty());

        // Another class's object is not a family parameter run at all.
        let other = set_object(set_classes().double, 10);
        assert!(read_family_parameters(&other, family_classes()).is_empty());
    }

    #[test]
    fn declared_sets_pair_their_values_by_declaration_order() {
        let classes = set_classes();
        // `ParamValueDouble` writes its value before its identifier, the other
        // three write the identifier first.
        let mut doubles = set_object(classes.double, 10);
        doubles.numbers = vec![1.5, 2.5];
        doubles.integers = vec![-1_155_261, 221_296];
        let mut integers = set_object(classes.integer, 30);
        integers.integers = vec![-1_114_242, 0, 221_298, 7];
        let mut text = set_object(classes.text, 50);
        text.integers = vec![-1_001_203];
        text.strings = vec!["153".to_owned()];
        let mut references = set_object(classes.reference, 70);
        references.integers = vec![-1_010_106, 417_563];

        let found =
            ParameterSets::from_objects(&[doubles, integers, text, references], classes).unwrap();
        assert_eq!(found.sets, 4);
        assert_eq!(found.offset, 10);
        assert_eq!(
            found.parameters,
            [
                Parameter {
                    id: -1_155_261,
                    value: ParameterValue::Double(1.5)
                },
                Parameter {
                    id: 221_296,
                    value: ParameterValue::Double(2.5)
                },
                Parameter {
                    id: -1_114_242,
                    value: ParameterValue::Integer(0)
                },
                Parameter {
                    id: 221_298,
                    value: ParameterValue::Integer(7)
                },
                Parameter {
                    id: -1_001_203,
                    value: ParameterValue::Text("153".to_owned())
                },
                Parameter {
                    id: -1_010_106,
                    value: ParameterValue::Reference(417_563)
                },
            ]
        );
    }

    #[test]
    fn a_set_whose_values_do_not_pair_up_is_read_as_empty() {
        let classes = set_classes();
        let mut text = set_object(classes.text, 10);
        text.integers = vec![-1_001_203, 221_298];
        text.strings = vec!["153".to_owned()];
        let found = ParameterSets::from_objects(&[text], classes).unwrap();
        assert_eq!(found.sets, 1);
        assert!(found.parameters.is_empty());
    }

    #[test]
    fn a_record_with_no_parameter_set_object_has_no_run() {
        let classes = set_classes();
        assert_eq!(
            ParameterSets::from_objects(&[set_object(1234, 10)], classes),
            None
        );
    }

    use super::*;

    fn text_bytes(value: &str) -> Vec<u8> {
        let units = value.encode_utf16().collect::<Vec<_>>();
        let mut bytes = u32::try_from(units.len()).unwrap().to_le_bytes().to_vec();
        for unit in units {
            bytes.extend(unit.to_le_bytes());
        }
        bytes
    }

    /// One integer set and one text set, as an element stores them.
    fn sets_fixture() -> Vec<u8> {
        let mut bytes = vec![0xaa_u8; 6];
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend((-1_114_242_i32).to_le_bytes());
        bytes.extend(0_i32.to_le_bytes());
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend((-1_001_203_i32).to_le_bytes());
        bytes.extend(text_bytes("153"));
        bytes.extend(221_298_i32.to_le_bytes());
        bytes.extend(text_bytes("3"));
        bytes
    }

    #[test]
    fn reads_an_integer_set_followed_by_a_text_set() {
        let bytes = sets_fixture();
        let found = ParameterSets::scan(&bytes, &|id| id == 221_298).unwrap();

        assert_eq!(found.offset, 6);
        assert_eq!(found.sets, 2);
        assert_eq!(found.parameters.len(), 3);
        assert_eq!(
            found.parameters[0],
            Parameter {
                id: -1_114_242,
                value: ParameterValue::Integer(0)
            }
        );
        assert_eq!(
            found.parameters[1],
            Parameter {
                id: -1_001_203,
                value: ParameterValue::Text("153".to_owned())
            }
        );
        assert_eq!(
            found.parameters[2],
            Parameter {
                id: 221_298,
                value: ParameterValue::Text("3".to_owned())
            }
        );
        assert!(found.parameters[0].is_built_in());
        assert!(!found.parameters[2].is_built_in());
    }

    #[test]
    fn reads_a_double_set() {
        let mut bytes = 3_u32.to_le_bytes().to_vec();
        bytes.extend(3.937_f64.to_le_bytes());
        bytes.extend((-1_002_000_i32).to_le_bytes());
        bytes.extend(0.5_f64.to_le_bytes());
        bytes.extend((-1_002_001_i32).to_le_bytes());
        bytes.extend(12.0_f64.to_le_bytes());
        bytes.extend((-1_002_002_i32).to_le_bytes());

        let found = ParameterSets::scan(&bytes, &|_| false).unwrap();
        assert_eq!(found.sets, 1);
        assert_eq!(found.parameters.len(), 3);
        assert_eq!(found.parameters[0].value, ParameterValue::Double(3.937));
        assert_eq!(found.encoded_bytes, bytes.len());
    }

    #[test]
    fn schema_bound_scan_uses_the_referenced_value_kinds() {
        let classes = ParameterSetClassIndexes {
            double: 0x1122,
            integer: 0x2233,
            text: 0x3344,
            reference: 0x4455,
        };
        let mut bytes = vec![0xff, 0xff, 0x22, 0x11];
        bytes.extend([0xff, 0xff, 0xff, 0xff, 0x44, 0x33]);
        let pointer_end = bytes.len();
        bytes.extend([0xaa, 0xbb]);
        let offset = bytes.len();
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(0.009_f64.to_le_bytes());
        bytes.extend(7_i32.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(8_i32.to_le_bytes());
        bytes.extend(text_bytes("3"));

        let found = ParameterSets::scan_schema_bound(&bytes, offset, pointer_end, classes, &|id| {
            matches!(id, 7 | 8)
        })
        .unwrap();
        assert_eq!(found.offset, offset);
        assert_eq!(found.sets, 2);
        assert_eq!(found.parameters[0].value, ParameterValue::Double(0.009));
        assert_eq!(
            found.parameters[1].value,
            ParameterValue::Text("3".to_owned())
        );

        assert!(ParameterSets::scan_schema_bound(&bytes, offset, 0, classes, &|_| true).is_none());
    }

    #[test]
    fn schema_bound_scan_rejects_an_ambiguous_run() {
        let classes = ParameterSetClassIndexes {
            double: 1,
            integer: 2,
            text: 3,
            reference: 4,
        };
        let mut bytes = vec![0xff, 0xff, 2, 0];
        let pointer_end = bytes.len();
        let run = [
            1_u32.to_le_bytes(),
            7_i32.to_le_bytes(),
            1_i32.to_le_bytes(),
        ]
        .concat();
        bytes.extend(&run);
        bytes.extend(&run);
        assert!(
            ParameterSets::scan_schema_bound(&bytes, pointer_end, pointer_end, classes, &|id| id
                == 7)
            .is_none()
        );
    }

    #[test]
    fn rejects_an_unknown_positive_identifier() {
        let mut bytes = 2_u32.to_le_bytes().to_vec();
        bytes.extend(7_i32.to_le_bytes());
        bytes.extend(1_i32.to_le_bytes());
        bytes.extend(8_i32.to_le_bytes());
        bytes.extend(1_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|_| false).is_none());
    }

    #[test]
    fn accepts_a_single_set_with_one_built_in_parameter() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-1_002_000_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        let found = ParameterSets::scan(&bytes, &|_| false).unwrap();
        assert_eq!(found.parameters.len(), 1);
        assert_eq!(found.parameters[0].value, ParameterValue::Integer(4));
    }

    #[test]
    fn verified_scan_rejects_a_built_in_code_missing_from_the_catalog() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-65_536_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        assert!(ParameterSets::scan_verified(&bytes, &|_| false).is_none());
    }

    #[test]
    fn rejects_one_unaccompanied_positive_parameter() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend(3.937_f64.to_le_bytes());
        bytes.extend(221_298_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|id| id == 221_298).is_none());
    }

    #[test]
    fn refuses_a_code_outside_the_built_in_window() {
        let mut bytes = 1_u32.to_le_bytes().to_vec();
        bytes.extend((-15_990_784_i32).to_le_bytes());
        bytes.extend(4_i32.to_le_bytes());
        assert!(ParameterSets::scan(&bytes, &|_| false).is_none());
    }

    #[test]
    fn reads_the_spec_from_a_parameter_definition() {
        let mut bytes = vec![0xff; 17];
        let offset = bytes.len();
        bytes.extend(text_bytes(
            "autodesk.spec.aec.structural:massPerUnitLength-1.0.0",
        ));
        bytes.extend([0; 5]);

        let found = ParameterSpec::scan(&bytes).unwrap();
        assert_eq!(found.offset, offset);
        assert_eq!(
            found.type_id,
            "autodesk.spec.aec.structural:massPerUnitLength-1.0.0"
        );
    }

    #[test]
    fn rejects_an_ambiguous_parameter_spec() {
        let mut bytes = text_bytes("autodesk.spec.aec:length-2.0.0");
        bytes.extend(text_bytes("autodesk.spec.aec:area-2.0.0"));
        assert!(ParameterSpec::scan(&bytes).is_none());
    }
}

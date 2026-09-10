#!/usr/bin/env python3
"""Generate the checked-in IFC4 product and type attribute table.

The exporter writes every attribute an entity declares, set or not, so it has
to know how many there are and which of them the schema requires a value for.
That was hand-written for the dozen entities the built-in mapping reaches; a
class mapping file may name any of them, so the whole schema is tabulated here
instead of guessed at.

Reads the IFC4 schema through IfcOpenShell, so the table is the schema's own
answer rather than a reading of the documentation:

    .venv/bin/python scripts/generate_ifc4_entities.py \
        > crates/ifc-export/src/ifc4_entities.rs
"""

from __future__ import annotations

import sys

import ifcopenshell
import ifcopenshell.util.pset

SCHEMA = "IFC4"
# What every `IfcElement` carries before its own attributes begin, and what
# every `IfcElementType` does. An entity's own attributes are the ones past
# these, and they are what the table holds.
ELEMENT_BASE = 8
TYPE_BASE = 9


def enumeration_members(attribute):
    """The members of an attribute's enumeration, or None where it is not one."""
    kind = attribute.type_of_attribute()
    declared = kind.declared_type() if hasattr(kind, "declared_type") else None
    if declared is not None and hasattr(declared, "enumeration_items"):
        return list(declared.enumeration_items())
    return None


def own_attributes(declaration, base: int):
    """The attributes an entity declares past the base ones, as
    (name, kind) where kind is 'optional', 'notdefined' or 'required'.

    An enumeration with a NOTDEFINED member is that whether or not it is
    optional: writing the member that says "not stated" states more than
    leaving the attribute unset, and it is what the exporter already wrote for
    the entities whose layout was hand-written."""
    rows = []
    attributes = declaration.all_attributes()
    for index, attribute in enumerate(attributes):
        if index < base:
            continue
        values = enumeration_members(attribute) or []
        if "NOTDEFINED" in values:
            rows.append((attribute.name(), "notdefined"))
        elif attribute.optional():
            rows.append((attribute.name(), "optional"))
        else:
            rows.append((attribute.name(), "required"))
    return rows


def descendants(declaration):
    for subtype in declaration.subtypes():
        yield subtype
        yield from descendants(subtype)


def predefined_type(declaration, base: int):
    """Where an entity's `PredefinedType` sits among its own attributes, and
    what the schema allows there."""
    for index, attribute in enumerate(declaration.all_attributes()):
        if index < base or attribute.name() != "PredefinedType":
            continue
        members = enumeration_members(attribute)
        if members is not None:
            return index - base, members
    return None


def table(name: str, root: str, base: int, schema) -> list[tuple[str, list, object]]:
    root_declaration = schema.declaration_by_name(root).as_entity()
    rows = []
    for declaration in sorted(descendants(root_declaration), key=lambda d: d.name()):
        if declaration.is_abstract():
            continue
        rows.append(
            (
                declaration.name(),
                own_attributes(declaration, base),
                predefined_type(declaration, base),
            )
        )
    return rows


def emit(rows, name: str, doc: str) -> None:
    print(f"/// {doc}")
    print(f"pub(crate) const {name}: &[Ifc4Entity] = &[")
    for entity, attributes, predefined in rows:
        kinds = ", ".join(f"Attribute::{kind.capitalize()}" for _, kind in attributes)
        names = ", ".join(f"`{attribute}`" for attribute, _ in attributes)
        if names:
            print(f"    // {names}")
        print(f'    Ifc4Entity {{ name: "{entity.upper()}", attributes: &[{kinds}],')
        if predefined is None:
            print("        predefined_type: None },")
        else:
            index, members = predefined
            listed = ", ".join(f'"{member}"' for member in members)
            print(
                f"        predefined_type: Some(PredefinedType {{ index: {index},"
                f" members: &[{listed}] }}) }},"
            )
    print("];")


def common_sets() -> list[tuple[str, str]]:
    """IFC's own `Pset_..Common` for one entity, from the buildingSMART
    property set templates: the sets that hold `Reference` and apply to
    exactly one entity. That is the property this exporter can fill, and the
    template is what says which set holds it for which entity."""
    rows = []
    for template in ifcopenshell.util.pset.get_template(SCHEMA).templates:
        for pset in template.by_type("IfcPropertySetTemplate"):
            if not pset.Name.endswith("Common"):
                continue
            entities = (pset.ApplicableEntity or "").split(",")
            if len(entities) != 1 or not entities[0].startswith("Ifc"):
                continue
            names = [
                property.Name for property in (pset.HasPropertyTemplates or [])
            ]
            if "Reference" in names:
                rows.append((entities[0].strip().upper(), pset.Name))
    return sorted(set(rows))


def base_quantity_sets() -> list[tuple[str, str, list[str]]]:
    """IFC's own base quantity set for one entity, from the same templates:
    the quantity sets that apply to exactly one entity. Which quantities the
    set holds is part of the row, because the exporter may only write the ones
    it has measured and the names differ from entity to entity."""
    rows = []
    for template in ifcopenshell.util.pset.get_template(SCHEMA).templates:
        for pset in template.by_type("IfcPropertySetTemplate"):
            if not (pset.TemplateType or "").startswith("QTO"):
                continue
            entities = (pset.ApplicableEntity or "").split(",")
            if len(entities) != 1 or not entities[0].startswith("Ifc"):
                continue
            names = [
                property.Name for property in (pset.HasPropertyTemplates or [])
            ]
            rows.append((entities[0].strip().upper(), pset.Name, names))
    return sorted(set((entity, name, tuple(names)) for entity, name, names in rows))


def emit_base_quantities() -> None:
    print("/// IFC's own base quantity set for an entity, and the quantities it")
    print("/// holds. The exporter writes only the ones it has measured.")
    print("pub(crate) const IFC4_BASE_QUANTITY_SETS: &[(&str, &str, &[&str])] = &[")
    for entity, name, names in base_quantity_sets():
        listed = ", ".join(f'"{quantity}"' for quantity in names)
        print(f'    ("{entity}", "{name}", &[{listed}]),')
    print("];")


def emit_common_sets() -> None:
    print("/// IFC's own `Pset_..Common` for an entity, where the property set")
    print("/// templates define one that holds `Reference`.")
    print("pub(crate) const IFC4_COMMON_PROPERTY_SETS: &[(&str, &str)] = &[")
    for entity, pset in common_sets():
        print(f'    ("{entity}", "{pset}"),')
    print("];")


def main() -> int:
    schema = ifcopenshell.ifcopenshell_wrapper.schema_by_name(SCHEMA)
    print("//! IFC4 entity attributes, generated from the schema itself.")
    print("//!")
    print("//! Do not edit. Regenerate with:")
    print("//!")
    print("//! ```text")
    print("//! .venv/bin/python scripts/generate_ifc4_entities.py \\")
    print("//!     > crates/ifc-export/src/ifc4_entities.rs")
    print("//! ```")
    print("//!")
    print("//! Each row is one instantiable entity and the attributes it declares")
    print("//! past the eight of `IfcElement` (or the nine of `IfcElementType`),")
    print("//! in order, with what the schema asks of each.")
    print()
    print("/// What the schema requires of one attribute.")
    print("//")
    print("// `Required` is not constructed by the IFC4 tables below - no")
    print("// instantiable element or type declares an attribute past the base")
    print("// ones that must be set and is not an enumeration. It is generated")
    print("// anyway, so that a schema which does declare one is written as")
    print("// something the exporter can see rather than silently omitted.")
    print("#[allow(dead_code)]")
    print("#[derive(Clone, Copy, Debug, Eq, PartialEq)]")
    print("pub(crate) enum Attribute {")
    print("    /// May be unset, and is written `$`.")
    print("    Optional,")
    print("    /// An enumeration with a `NOTDEFINED` member, optional or not:")
    print("    /// the member that says the value is not stated says more than")
    print("    /// leaving the attribute out, so it is written.")
    print("    Notdefined,")
    print("    /// Required, with no value this exporter can supply.")
    print("    Required,")
    print("}")
    print()
    print("/// Where an entity states what kind of thing it is, and what the")
    print("/// schema allows it to say. A class mapping file may name one of")
    print("/// these members; anything else is refused rather than written.")
    print("#[derive(Clone, Copy, Debug, Eq, PartialEq)]")
    print("pub(crate) struct PredefinedType {")
    print("    /// Index into `Ifc4Entity::attributes`.")
    print("    pub(crate) index: usize,")
    print("    pub(crate) members: &'static [&'static str],")
    print("}")
    print()
    print("#[derive(Clone, Copy, Debug, Eq, PartialEq)]")
    print("pub(crate) struct Ifc4Entity {")
    print("    pub(crate) name: &'static str,")
    print("    pub(crate) attributes: &'static [Attribute],")
    print("    pub(crate) predefined_type: Option<PredefinedType>,")
    print("}")
    print()
    emit(
        table("products", "IfcElement", ELEMENT_BASE, schema),
        "IFC4_ELEMENTS",
        "Every instantiable `IfcElement`, and what it declares past `Tag`.",
    )
    print()
    emit(
        table("types", "IfcElementType", TYPE_BASE, schema),
        "IFC4_ELEMENT_TYPES",
        "Every instantiable `IfcElementType`, past `ElementType`.",
    )
    print()
    emit(
        table("spatial", "IfcSpatialElementType", TYPE_BASE, schema),
        "IFC4_SPATIAL_ELEMENT_TYPES",
        "Every instantiable `IfcSpatialElementType`, past `ElementType`.",
    )
    print()
    emit_common_sets()
    print()
    emit_base_quantities()
    return 0


if __name__ == "__main__":
    sys.exit(main())

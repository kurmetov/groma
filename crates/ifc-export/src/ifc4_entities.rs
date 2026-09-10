//! IFC4 entity attributes, generated from the schema itself.
//!
//! Do not edit. Regenerate with:
//!
//! ```text
//! .venv/bin/python scripts/generate_ifc4_entities.py \
//!     > crates/ifc-export/src/ifc4_entities.rs
//! ```
//!
//! Each row is one instantiable entity and the attributes it declares
//! past the eight of `IfcElement` (or the nine of `IfcElementType`),
//! in order, with what the schema asks of each.

/// What the schema requires of one attribute.
//
// `Required` is not constructed by the IFC4 tables below - no
// instantiable element or type declares an attribute past the base
// ones that must be set and is not an enumeration. It is generated
// anyway, so that a schema which does declare one is written as
// something the exporter can see rather than silently omitted.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Attribute {
    /// May be unset, and is written `$`.
    Optional,
    /// An enumeration with a `NOTDEFINED` member, optional or not:
    /// the member that says the value is not stated says more than
    /// leaving the attribute out, so it is written.
    Notdefined,
    /// Required, with no value this exporter can supply.
    Required,
}

/// Where an entity states what kind of thing it is, and what the
/// schema allows it to say. A class mapping file may name one of
/// these members; anything else is refused rather than written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PredefinedType {
    /// Index into `Ifc4Entity::attributes`.
    pub(crate) index: usize,
    pub(crate) members: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ifc4Entity {
    pub(crate) name: &'static str,
    pub(crate) attributes: &'static [Attribute],
    pub(crate) predefined_type: Option<PredefinedType>,
}

/// Every instantiable `IfcElement`, and what it declares past `Tag`.
pub(crate) const IFC4_ELEMENTS: &[Ifc4Entity] = &[
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCACTUATOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ELECTRICACTUATOR",
                "HANDOPERATEDACTUATOR",
                "HYDRAULICACTUATOR",
                "PNEUMATICACTUATOR",
                "THERMOSTATICACTUATOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAIRTERMINAL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DIFFUSER",
                "GRILLE",
                "LOUVRE",
                "REGISTER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAIRTERMINALBOX",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONSTANTFLOW",
                "VARIABLEFLOWPRESSUREDEPENDANT",
                "VARIABLEFLOWPRESSUREINDEPENDANT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAIRTOAIRHEATRECOVERY",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FIXEDPLATECOUNTERFLOWEXCHANGER",
                "FIXEDPLATECROSSFLOWEXCHANGER",
                "FIXEDPLATEPARALLELFLOWEXCHANGER",
                "ROTARYWHEEL",
                "RUNAROUNDCOILLOOP",
                "HEATPIPE",
                "TWINTOWERENTHALPYRECOVERYLOOPS",
                "THERMOSIPHONSEALEDTUBEHEATEXCHANGERS",
                "THERMOSIPHONCOILTYPEHEATEXCHANGERS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCALARM",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BELL",
                "BREAKGLASSBUTTON",
                "LIGHT",
                "MANUALPULLBOX",
                "SIREN",
                "WHISTLE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAUDIOVISUALAPPLIANCE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AMPLIFIER",
                "CAMERA",
                "DISPLAY",
                "MICROPHONE",
                "PLAYER",
                "PROJECTOR",
                "RECEIVER",
                "SPEAKER",
                "SWITCHER",
                "TELEPHONE",
                "TUNER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBEAM",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEAM",
                "JOIST",
                "HOLLOWCORE",
                "LINTEL",
                "SPANDREL",
                "T_BEAM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBEAMSTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEAM",
                "JOIST",
                "HOLLOWCORE",
                "LINTEL",
                "SPANDREL",
                "T_BEAM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBOILER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["WATER", "STEAM", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBUILDINGELEMENTPART",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["INSULATION", "PRECASTPANEL", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBUILDINGELEMENTPROXY",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COMPLEX",
                "ELEMENT",
                "PARTIAL",
                "PROVISIONFORVOID",
                "PROVISIONFORSPACE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBURNER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLECARRIERFITTING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEND",
                "CROSS",
                "REDUCER",
                "TEE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLECARRIERSEGMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CABLELADDERSEGMENT",
                "CABLETRAYSEGMENT",
                "CABLETRUNKINGSEGMENT",
                "CONDUITSEGMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLEFITTING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONNECTOR",
                "ENTRY",
                "EXIT",
                "JUNCTION",
                "TRANSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLESEGMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BUSBARSEGMENT",
                "CABLESEGMENT",
                "CONDUCTORSEGMENT",
                "CORESEGMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCHILLER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRCOOLED",
                "WATERCOOLED",
                "HEATRECOVERY",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCHIMNEY",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    Ifc4Entity {
        name: "IFCCIVILELEMENT",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOIL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DXCOOLINGCOIL",
                "ELECTRICHEATINGCOIL",
                "GASHEATINGCOIL",
                "HYDRONICCOIL",
                "STEAMHEATINGCOIL",
                "WATERCOOLINGCOIL",
                "WATERHEATINGCOIL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOLUMN",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["COLUMN", "PILASTER", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOLUMNSTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["COLUMN", "PILASTER", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOMMUNICATIONSAPPLIANCE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ANTENNA",
                "COMPUTER",
                "FAX",
                "GATEWAY",
                "MODEM",
                "NETWORKAPPLIANCE",
                "NETWORKBRIDGE",
                "NETWORKHUB",
                "PRINTER",
                "REPEATER",
                "ROUTER",
                "SCANNER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOMPRESSOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DYNAMIC",
                "RECIPROCATING",
                "ROTARY",
                "SCROLL",
                "TROCHOIDAL",
                "SINGLESTAGE",
                "BOOSTER",
                "OPENTYPE",
                "HERMETIC",
                "SEMIHERMETIC",
                "WELDEDSHELLHERMETIC",
                "ROLLINGPISTON",
                "ROTARYVANE",
                "SINGLESCREW",
                "TWINSCREW",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCONDENSER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRCOOLED",
                "EVAPORATIVECOOLED",
                "WATERCOOLED",
                "WATERCOOLEDBRAZEDPLATE",
                "WATERCOOLEDSHELLCOIL",
                "WATERCOOLEDSHELLTUBE",
                "WATERCOOLEDTUBEINTUBE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCONTROLLER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOATING",
                "PROGRAMMABLE",
                "PROPORTIONAL",
                "MULTIPOSITION",
                "TWOPOSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOOLEDBEAM",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["ACTIVE", "PASSIVE", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOOLINGTOWER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "NATURALDRAFT",
                "MECHANICALINDUCEDDRAFT",
                "MECHANICALFORCEDDRAFT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOVERING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CEILING",
                "FLOORING",
                "CLADDING",
                "ROOFING",
                "MOLDING",
                "SKIRTINGBOARD",
                "INSULATION",
                "MEMBRANE",
                "SLEEVING",
                "WRAPPING",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCURTAINWALL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDAMPER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BACKDRAFTDAMPER",
                "BALANCINGDAMPER",
                "BLASTDAMPER",
                "CONTROLDAMPER",
                "FIREDAMPER",
                "FIRESMOKEDAMPER",
                "FUMEHOODEXHAUST",
                "GRAVITYDAMPER",
                "GRAVITYRELIEFDAMPER",
                "RELIEFDAMPER",
                "SMOKEDAMPER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDISCRETEACCESSORY",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ANCHORPLATE",
                "BRACKET",
                "SHOE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDISTRIBUTIONCHAMBERELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FORMEDDUCT",
                "INSPECTIONCHAMBER",
                "INSPECTIONPIT",
                "MANHOLE",
                "METERCHAMBER",
                "SUMP",
                "TRENCH",
                "VALVECHAMBER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCDISTRIBUTIONCONTROLELEMENT",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCDISTRIBUTIONELEMENT",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCDISTRIBUTIONFLOWELEMENT",
        attributes: &[],
        predefined_type: None,
    },
    // `OverallHeight`, `OverallWidth`, `PredefinedType`, `OperationType`, `UserDefinedOperationType`
    Ifc4Entity {
        name: "IFCDOOR",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
            Attribute::Notdefined,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 2,
            members: &["DOOR", "GATE", "TRAPDOOR", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `OverallHeight`, `OverallWidth`, `PredefinedType`, `OperationType`, `UserDefinedOperationType`
    Ifc4Entity {
        name: "IFCDOORSTANDARDCASE",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
            Attribute::Notdefined,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 2,
            members: &["DOOR", "GATE", "TRAPDOOR", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDUCTFITTING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEND",
                "CONNECTOR",
                "ENTRY",
                "EXIT",
                "JUNCTION",
                "OBSTRUCTION",
                "TRANSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDUCTSEGMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "RIGIDSEGMENT",
                "FLEXIBLESEGMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDUCTSILENCER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLATOVAL",
                "RECTANGULAR",
                "ROUND",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICAPPLIANCE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DISHWASHER",
                "ELECTRICCOOKER",
                "FREESTANDINGELECTRICHEATER",
                "FREESTANDINGFAN",
                "FREESTANDINGWATERHEATER",
                "FREESTANDINGWATERCOOLER",
                "FREEZER",
                "FRIDGE_FREEZER",
                "HANDDRYER",
                "KITCHENMACHINE",
                "MICROWAVE",
                "PHOTOCOPIER",
                "REFRIGERATOR",
                "TUMBLEDRYER",
                "VENDINGMACHINE",
                "WASHINGMACHINE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICDISTRIBUTIONBOARD",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONSUMERUNIT",
                "DISTRIBUTIONBOARD",
                "MOTORCONTROLCENTRE",
                "SWITCHBOARD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICFLOWSTORAGEDEVICE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BATTERY",
                "CAPACITORBANK",
                "HARMONICFILTER",
                "INDUCTORBANK",
                "UPS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICGENERATOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CHP",
                "ENGINEGENERATOR",
                "STANDALONE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICMOTOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DC",
                "INDUCTION",
                "POLYPHASE",
                "RELUCTANCESYNCHRONOUS",
                "SYNCHRONOUS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICTIMECONTROL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "TIMECLOCK",
                "TIMEDELAY",
                "RELAY",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `AssemblyPlace`, `PredefinedType`
    Ifc4Entity {
        name: "IFCELEMENTASSEMBLY",
        attributes: &[Attribute::Notdefined, Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 1,
            members: &[
                "ACCESSORY_ASSEMBLY",
                "ARCH",
                "BEAM_GRID",
                "BRACED_FRAME",
                "GIRDER",
                "REINFORCEMENT_UNIT",
                "RIGID_FRAME",
                "SLAB_FIELD",
                "TRUSS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCENERGYCONVERSIONDEVICE",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCENGINE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "EXTERNALCOMBUSTION",
                "INTERNALCOMBUSTION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCEVAPORATIVECOOLER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DIRECTEVAPORATIVERANDOMMEDIAAIRCOOLER",
                "DIRECTEVAPORATIVERIGIDMEDIAAIRCOOLER",
                "DIRECTEVAPORATIVESLINGERSPACKAGEDAIRCOOLER",
                "DIRECTEVAPORATIVEPACKAGEDROTARYAIRCOOLER",
                "DIRECTEVAPORATIVEAIRWASHER",
                "INDIRECTEVAPORATIVEPACKAGEAIRCOOLER",
                "INDIRECTEVAPORATIVEWETCOIL",
                "INDIRECTEVAPORATIVECOOLINGTOWERORCOILCOOLER",
                "INDIRECTDIRECTCOMBINATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCEVAPORATOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DIRECTEXPANSION",
                "DIRECTEXPANSIONSHELLANDTUBE",
                "DIRECTEXPANSIONTUBEINTUBE",
                "DIRECTEXPANSIONBRAZEDPLATE",
                "FLOODEDSHELLANDTUBE",
                "SHELLANDCOIL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFAN",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CENTRIFUGALFORWARDCURVED",
                "CENTRIFUGALRADIAL",
                "CENTRIFUGALBACKWARDINCLINEDCURVED",
                "CENTRIFUGALAIRFOIL",
                "TUBEAXIAL",
                "VANEAXIAL",
                "PROPELLORAXIAL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFASTENER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["GLUE", "MORTAR", "WELD", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFILTER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRPARTICLEFILTER",
                "COMPRESSEDAIRFILTER",
                "ODORFILTER",
                "OILFILTER",
                "STRAINER",
                "WATERFILTER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFIRESUPPRESSIONTERMINAL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BREECHINGINLET",
                "FIREHYDRANT",
                "HOSEREEL",
                "SPRINKLER",
                "SPRINKLERDEFLECTOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCFLOWCONTROLLER",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCFLOWFITTING",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFLOWINSTRUMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "PRESSUREGAUGE",
                "THERMOMETER",
                "AMMETER",
                "FREQUENCYMETER",
                "POWERFACTORMETER",
                "PHASEANGLEMETER",
                "VOLTMETER_PEAK",
                "VOLTMETER_RMS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFLOWMETER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ENERGYMETER",
                "GASMETER",
                "OILMETER",
                "WATERMETER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCFLOWMOVINGDEVICE",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCFLOWSEGMENT",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCFLOWSTORAGEDEVICE",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCFLOWTERMINAL",
        attributes: &[],
        predefined_type: None,
    },
    Ifc4Entity {
        name: "IFCFLOWTREATMENTDEVICE",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFOOTING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CAISSON_FOUNDATION",
                "FOOTING_BEAM",
                "PAD_FOOTING",
                "PILE_CAP",
                "STRIP_FOOTING",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCFURNISHINGELEMENT",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFURNITURE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CHAIR",
                "TABLE",
                "DESK",
                "BED",
                "FILECABINET",
                "SHELF",
                "SOFA",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCGEOGRAPHICELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["TERRAIN", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCHEATEXCHANGER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["PLATE", "SHELLANDTUBE", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCHUMIDIFIER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STEAMINJECTION",
                "ADIABATICAIRWASHER",
                "ADIABATICPAN",
                "ADIABATICWETTEDELEMENT",
                "ADIABATICATOMIZING",
                "ADIABATICULTRASONIC",
                "ADIABATICRIGIDMEDIA",
                "ADIABATICCOMPRESSEDAIRNOZZLE",
                "ASSISTEDELECTRIC",
                "ASSISTEDNATURALGAS",
                "ASSISTEDPROPANE",
                "ASSISTEDBUTANE",
                "ASSISTEDSTEAM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCINTERCEPTOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CYCLONIC",
                "GREASE",
                "OIL",
                "PETROL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCJUNCTIONBOX",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["DATA", "POWER", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCLAMP",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COMPACTFLUORESCENT",
                "FLUORESCENT",
                "HALOGEN",
                "HIGHPRESSUREMERCURY",
                "HIGHPRESSURESODIUM",
                "LED",
                "METALHALIDE",
                "OLED",
                "TUNGSTENFILAMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCLIGHTFIXTURE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "POINTSOURCE",
                "DIRECTIONSOURCE",
                "SECURITYLIGHTING",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `NominalDiameter`, `NominalLength`, `PredefinedType`
    Ifc4Entity {
        name: "IFCMECHANICALFASTENER",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
        ],
        predefined_type: Some(PredefinedType {
            index: 2,
            members: &[
                "ANCHORBOLT",
                "BOLT",
                "DOWEL",
                "NAIL",
                "NAILPLATE",
                "RIVET",
                "SCREW",
                "SHEARCONNECTOR",
                "STAPLE",
                "STUDSHEARCONNECTOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMEDICALDEVICE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRSTATION",
                "FEEDAIRUNIT",
                "OXYGENGENERATOR",
                "OXYGENPLANT",
                "VACUUMSTATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMEMBER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BRACE",
                "CHORD",
                "COLLAR",
                "MEMBER",
                "MULLION",
                "PLATE",
                "POST",
                "PURLIN",
                "RAFTER",
                "STRINGER",
                "STRUT",
                "STUD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMEMBERSTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BRACE",
                "CHORD",
                "COLLAR",
                "MEMBER",
                "MULLION",
                "PLATE",
                "POST",
                "PURLIN",
                "RAFTER",
                "STRINGER",
                "STRUT",
                "STUD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMOTORCONNECTION",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BELTDRIVE",
                "COUPLING",
                "DIRECTDRIVE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCOPENINGELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["OPENING", "RECESS", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCOPENINGSTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["OPENING", "RECESS", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCOUTLET",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AUDIOVISUALOUTLET",
                "COMMUNICATIONSOUTLET",
                "POWEROUTLET",
                "DATAOUTLET",
                "TELEPHONEOUTLET",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `ConstructionType`
    Ifc4Entity {
        name: "IFCPILE",
        attributes: &[Attribute::Notdefined, Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BORED",
                "DRIVEN",
                "JETGROUTING",
                "COHESION",
                "FRICTION",
                "SUPPORT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPIPEFITTING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEND",
                "CONNECTOR",
                "ENTRY",
                "EXIT",
                "JUNCTION",
                "OBSTRUCTION",
                "TRANSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPIPESEGMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CULVERT",
                "FLEXIBLESEGMENT",
                "RIGIDSEGMENT",
                "GUTTER",
                "SPOOL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPLATE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["CURTAIN_PANEL", "SHEET", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPLATESTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["CURTAIN_PANEL", "SHEET", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPROJECTIONELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPROTECTIVEDEVICE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CIRCUITBREAKER",
                "EARTHLEAKAGECIRCUITBREAKER",
                "EARTHINGSWITCH",
                "FUSEDISCONNECTOR",
                "RESIDUALCURRENTCIRCUITBREAKER",
                "RESIDUALCURRENTSWITCH",
                "VARISTOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPROTECTIVEDEVICETRIPPINGUNIT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ELECTRONIC",
                "ELECTROMAGNETIC",
                "RESIDUALCURRENT",
                "THERMAL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPUMP",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CIRCULATOR",
                "ENDSUCTION",
                "SPLITCASE",
                "SUBMERSIBLEPUMP",
                "SUMPPUMP",
                "VERTICALINLINE",
                "VERTICALTURBINE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCRAILING",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "HANDRAIL",
                "GUARDRAIL",
                "BALUSTRADE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCRAMP",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STRAIGHT_RUN_RAMP",
                "TWO_STRAIGHT_RUN_RAMP",
                "QUARTER_TURN_RAMP",
                "TWO_QUARTER_TURN_RAMP",
                "HALF_TURN_RAMP",
                "SPIRAL_RAMP",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCRAMPFLIGHT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["STRAIGHT", "SPIRAL", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `SteelGrade`, `NominalDiameter`, `CrossSectionArea`, `BarLength`, `PredefinedType`, `BarSurface`
    Ifc4Entity {
        name: "IFCREINFORCINGBAR",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 4,
            members: &[
                "ANCHORING",
                "EDGE",
                "LIGATURE",
                "MAIN",
                "PUNCHING",
                "RING",
                "SHEAR",
                "STUD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `SteelGrade`, `MeshLength`, `MeshWidth`, `LongitudinalBarNominalDiameter`, `TransverseBarNominalDiameter`, `LongitudinalBarCrossSectionArea`, `TransverseBarCrossSectionArea`, `LongitudinalBarSpacing`, `TransverseBarSpacing`, `PredefinedType`
    Ifc4Entity {
        name: "IFCREINFORCINGMESH",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
        ],
        predefined_type: Some(PredefinedType {
            index: 9,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCROOF",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLAT_ROOF",
                "SHED_ROOF",
                "GABLE_ROOF",
                "HIP_ROOF",
                "HIPPED_GABLE_ROOF",
                "GAMBREL_ROOF",
                "MANSARD_ROOF",
                "BARREL_ROOF",
                "RAINBOW_ROOF",
                "BUTTERFLY_ROOF",
                "PAVILION_ROOF",
                "DOME_ROOF",
                "FREEFORM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSANITARYTERMINAL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BATH",
                "BIDET",
                "CISTERN",
                "SHOWER",
                "SINK",
                "SANITARYFOUNTAIN",
                "TOILETPAN",
                "URINAL",
                "WASHHANDBASIN",
                "WCSEAT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSENSOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COSENSOR",
                "CO2SENSOR",
                "CONDUCTANCESENSOR",
                "CONTACTSENSOR",
                "FIRESENSOR",
                "FLOWSENSOR",
                "FROSTSENSOR",
                "GASSENSOR",
                "HEATSENSOR",
                "HUMIDITYSENSOR",
                "IDENTIFIERSENSOR",
                "IONCONCENTRATIONSENSOR",
                "LEVELSENSOR",
                "LIGHTSENSOR",
                "MOISTURESENSOR",
                "MOVEMENTSENSOR",
                "PHSENSOR",
                "PRESSURESENSOR",
                "RADIATIONSENSOR",
                "RADIOACTIVITYSENSOR",
                "SMOKESENSOR",
                "SOUNDSENSOR",
                "TEMPERATURESENSOR",
                "WINDSENSOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSHADINGDEVICE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["JALOUSIE", "SHUTTER", "AWNING", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSLAB",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOOR",
                "ROOF",
                "LANDING",
                "BASESLAB",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSLABELEMENTEDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOOR",
                "ROOF",
                "LANDING",
                "BASESLAB",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSLABSTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOOR",
                "ROOF",
                "LANDING",
                "BASESLAB",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSOLARDEVICE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["SOLARCOLLECTOR", "SOLARPANEL", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSPACEHEATER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["CONVECTOR", "RADIATOR", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSTACKTERMINAL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BIRDCAGE",
                "COWL",
                "RAINWATERHOPPER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSTAIR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STRAIGHT_RUN_STAIR",
                "TWO_STRAIGHT_RUN_STAIR",
                "QUARTER_WINDING_STAIR",
                "QUARTER_TURN_STAIR",
                "HALF_WINDING_STAIR",
                "HALF_TURN_STAIR",
                "TWO_QUARTER_WINDING_STAIR",
                "TWO_QUARTER_TURN_STAIR",
                "THREE_QUARTER_WINDING_STAIR",
                "THREE_QUARTER_TURN_STAIR",
                "SPIRAL_STAIR",
                "DOUBLE_RETURN_STAIR",
                "CURVED_RUN_STAIR",
                "TWO_CURVED_RUN_STAIR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `NumberOfRisers`, `NumberOfTreads`, `RiserHeight`, `TreadLength`, `PredefinedType`
    Ifc4Entity {
        name: "IFCSTAIRFLIGHT",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
        ],
        predefined_type: Some(PredefinedType {
            index: 4,
            members: &[
                "STRAIGHT",
                "WINDER",
                "SPIRAL",
                "CURVED",
                "FREEFORM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSURFACEFEATURE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["MARK", "TAG", "TREATMENT", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSWITCHINGDEVICE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONTACTOR",
                "DIMMERSWITCH",
                "EMERGENCYSTOP",
                "KEYPAD",
                "MOMENTARYSWITCH",
                "SELECTORSWITCH",
                "STARTER",
                "SWITCHDISCONNECTOR",
                "TOGGLESWITCH",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSYSTEMFURNITUREELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["PANEL", "WORKSURFACE", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTANK",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BASIN",
                "BREAKPRESSURE",
                "EXPANSION",
                "FEEDANDEXPANSION",
                "PRESSUREVESSEL",
                "STORAGE",
                "VESSEL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `SteelGrade`, `PredefinedType`, `NominalDiameter`, `CrossSectionArea`, `TensionForce`, `PreStress`, `FrictionCoefficient`, `AnchorageSlip`, `MinCurvatureRadius`
    Ifc4Entity {
        name: "IFCTENDON",
        attributes: &[
            Attribute::Optional,
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 1,
            members: &[
                "BAR",
                "COATED",
                "STRAND",
                "WIRE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `SteelGrade`, `PredefinedType`
    Ifc4Entity {
        name: "IFCTENDONANCHOR",
        attributes: &[Attribute::Optional, Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 1,
            members: &[
                "COUPLER",
                "FIXED_END",
                "TENSIONING_END",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTRANSFORMER",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CURRENT",
                "FREQUENCY",
                "INVERTER",
                "RECTIFIER",
                "VOLTAGE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTRANSPORTELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ELEVATOR",
                "ESCALATOR",
                "MOVINGWALKWAY",
                "CRANEWAY",
                "LIFTINGGEAR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTUBEBUNDLE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["FINNED", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCUNITARYCONTROLELEMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ALARMPANEL",
                "CONTROLPANEL",
                "GASDETECTIONPANEL",
                "INDICATORPANEL",
                "MIMICPANEL",
                "HUMIDISTAT",
                "THERMOSTAT",
                "WEATHERSTATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCUNITARYEQUIPMENT",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRHANDLER",
                "AIRCONDITIONINGUNIT",
                "DEHUMIDIFIER",
                "SPLITSYSTEM",
                "ROOFTOPUNIT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCVALVE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRRELEASE",
                "ANTIVACUUM",
                "CHANGEOVER",
                "CHECK",
                "COMMISSIONING",
                "DIVERTING",
                "DRAWOFFCOCK",
                "DOUBLECHECK",
                "DOUBLEREGULATING",
                "FAUCET",
                "FLUSHING",
                "GASCOCK",
                "GASTAP",
                "ISOLATING",
                "MIXING",
                "PRESSUREREDUCING",
                "PRESSURERELIEF",
                "REGULATING",
                "SAFETYCUTOFF",
                "STEAMTRAP",
                "STOPCOCK",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCVIBRATIONISOLATOR",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["COMPRESSION", "SPRING", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    Ifc4Entity {
        name: "IFCVIRTUALELEMENT",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCVOIDINGFEATURE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CUTOUT",
                "NOTCH",
                "HOLE",
                "MITER",
                "CHAMFER",
                "EDGE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCWALL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "MOVABLE",
                "PARAPET",
                "PARTITIONING",
                "PLUMBINGWALL",
                "SHEAR",
                "SOLIDWALL",
                "STANDARD",
                "POLYGONAL",
                "ELEMENTEDWALL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCWALLELEMENTEDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "MOVABLE",
                "PARAPET",
                "PARTITIONING",
                "PLUMBINGWALL",
                "SHEAR",
                "SOLIDWALL",
                "STANDARD",
                "POLYGONAL",
                "ELEMENTEDWALL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCWALLSTANDARDCASE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "MOVABLE",
                "PARAPET",
                "PARTITIONING",
                "PLUMBINGWALL",
                "SHEAR",
                "SOLIDWALL",
                "STANDARD",
                "POLYGONAL",
                "ELEMENTEDWALL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCWASTETERMINAL",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOORTRAP",
                "FLOORWASTE",
                "GULLYSUMP",
                "GULLYTRAP",
                "ROOFDRAIN",
                "WASTEDISPOSALUNIT",
                "WASTETRAP",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `OverallHeight`, `OverallWidth`, `PredefinedType`, `PartitioningType`, `UserDefinedPartitioningType`
    Ifc4Entity {
        name: "IFCWINDOW",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
            Attribute::Notdefined,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 2,
            members: &[
                "WINDOW",
                "SKYLIGHT",
                "LIGHTDOME",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `OverallHeight`, `OverallWidth`, `PredefinedType`, `PartitioningType`, `UserDefinedPartitioningType`
    Ifc4Entity {
        name: "IFCWINDOWSTANDARDCASE",
        attributes: &[
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Notdefined,
            Attribute::Notdefined,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 2,
            members: &[
                "WINDOW",
                "SKYLIGHT",
                "LIGHTDOME",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
];

/// Every instantiable `IfcElementType`, past `ElementType`.
pub(crate) const IFC4_ELEMENT_TYPES: &[Ifc4Entity] = &[
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCACTUATORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ELECTRICACTUATOR",
                "HANDOPERATEDACTUATOR",
                "HYDRAULICACTUATOR",
                "PNEUMATICACTUATOR",
                "THERMOSTATICACTUATOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAIRTERMINALBOXTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONSTANTFLOW",
                "VARIABLEFLOWPRESSUREDEPENDANT",
                "VARIABLEFLOWPRESSUREINDEPENDANT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAIRTERMINALTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DIFFUSER",
                "GRILLE",
                "LOUVRE",
                "REGISTER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAIRTOAIRHEATRECOVERYTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FIXEDPLATECOUNTERFLOWEXCHANGER",
                "FIXEDPLATECROSSFLOWEXCHANGER",
                "FIXEDPLATEPARALLELFLOWEXCHANGER",
                "ROTARYWHEEL",
                "RUNAROUNDCOILLOOP",
                "HEATPIPE",
                "TWINTOWERENTHALPYRECOVERYLOOPS",
                "THERMOSIPHONSEALEDTUBEHEATEXCHANGERS",
                "THERMOSIPHONCOILTYPEHEATEXCHANGERS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCALARMTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BELL",
                "BREAKGLASSBUTTON",
                "LIGHT",
                "MANUALPULLBOX",
                "SIREN",
                "WHISTLE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCAUDIOVISUALAPPLIANCETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AMPLIFIER",
                "CAMERA",
                "DISPLAY",
                "MICROPHONE",
                "PLAYER",
                "PROJECTOR",
                "RECEIVER",
                "SPEAKER",
                "SWITCHER",
                "TELEPHONE",
                "TUNER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBEAMTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEAM",
                "JOIST",
                "HOLLOWCORE",
                "LINTEL",
                "SPANDREL",
                "T_BEAM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBOILERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["WATER", "STEAM", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBUILDINGELEMENTPARTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["INSULATION", "PRECASTPANEL", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBUILDINGELEMENTPROXYTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COMPLEX",
                "ELEMENT",
                "PARTIAL",
                "PROVISIONFORVOID",
                "PROVISIONFORSPACE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCBURNERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLECARRIERFITTINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEND",
                "CROSS",
                "REDUCER",
                "TEE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLECARRIERSEGMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CABLELADDERSEGMENT",
                "CABLETRAYSEGMENT",
                "CABLETRUNKINGSEGMENT",
                "CONDUITSEGMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLEFITTINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONNECTOR",
                "ENTRY",
                "EXIT",
                "JUNCTION",
                "TRANSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCABLESEGMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BUSBARSEGMENT",
                "CABLESEGMENT",
                "CONDUCTORSEGMENT",
                "CORESEGMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCHILLERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRCOOLED",
                "WATERCOOLED",
                "HEATRECOVERY",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCHIMNEYTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    Ifc4Entity {
        name: "IFCCIVILELEMENTTYPE",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOILTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DXCOOLINGCOIL",
                "ELECTRICHEATINGCOIL",
                "GASHEATINGCOIL",
                "HYDRONICCOIL",
                "STEAMHEATINGCOIL",
                "WATERCOOLINGCOIL",
                "WATERHEATINGCOIL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOLUMNTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["COLUMN", "PILASTER", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOMMUNICATIONSAPPLIANCETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ANTENNA",
                "COMPUTER",
                "FAX",
                "GATEWAY",
                "MODEM",
                "NETWORKAPPLIANCE",
                "NETWORKBRIDGE",
                "NETWORKHUB",
                "PRINTER",
                "REPEATER",
                "ROUTER",
                "SCANNER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOMPRESSORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DYNAMIC",
                "RECIPROCATING",
                "ROTARY",
                "SCROLL",
                "TROCHOIDAL",
                "SINGLESTAGE",
                "BOOSTER",
                "OPENTYPE",
                "HERMETIC",
                "SEMIHERMETIC",
                "WELDEDSHELLHERMETIC",
                "ROLLINGPISTON",
                "ROTARYVANE",
                "SINGLESCREW",
                "TWINSCREW",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCONDENSERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRCOOLED",
                "EVAPORATIVECOOLED",
                "WATERCOOLED",
                "WATERCOOLEDBRAZEDPLATE",
                "WATERCOOLEDSHELLCOIL",
                "WATERCOOLEDSHELLTUBE",
                "WATERCOOLEDTUBEINTUBE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCONTROLLERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOATING",
                "PROGRAMMABLE",
                "PROPORTIONAL",
                "MULTIPOSITION",
                "TWOPOSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOOLEDBEAMTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["ACTIVE", "PASSIVE", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOOLINGTOWERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "NATURALDRAFT",
                "MECHANICALINDUCEDDRAFT",
                "MECHANICALFORCEDDRAFT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCOVERINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CEILING",
                "FLOORING",
                "CLADDING",
                "ROOFING",
                "MOLDING",
                "SKIRTINGBOARD",
                "INSULATION",
                "MEMBRANE",
                "SLEEVING",
                "WRAPPING",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCCURTAINWALLTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDAMPERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BACKDRAFTDAMPER",
                "BALANCINGDAMPER",
                "BLASTDAMPER",
                "CONTROLDAMPER",
                "FIREDAMPER",
                "FIRESMOKEDAMPER",
                "FUMEHOODEXHAUST",
                "GRAVITYDAMPER",
                "GRAVITYRELIEFDAMPER",
                "RELIEFDAMPER",
                "SMOKEDAMPER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDISCRETEACCESSORYTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ANCHORPLATE",
                "BRACKET",
                "SHOE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDISTRIBUTIONCHAMBERELEMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FORMEDDUCT",
                "INSPECTIONCHAMBER",
                "INSPECTIONPIT",
                "MANHOLE",
                "METERCHAMBER",
                "SUMP",
                "TRENCH",
                "VALVECHAMBER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCDISTRIBUTIONELEMENTTYPE",
        attributes: &[],
        predefined_type: None,
    },
    // `PredefinedType`, `OperationType`, `ParameterTakesPrecedence`, `UserDefinedOperationType`
    Ifc4Entity {
        name: "IFCDOORTYPE",
        attributes: &[
            Attribute::Notdefined,
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["DOOR", "GATE", "TRAPDOOR", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDUCTFITTINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEND",
                "CONNECTOR",
                "ENTRY",
                "EXIT",
                "JUNCTION",
                "OBSTRUCTION",
                "TRANSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDUCTSEGMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "RIGIDSEGMENT",
                "FLEXIBLESEGMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCDUCTSILENCERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLATOVAL",
                "RECTANGULAR",
                "ROUND",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICAPPLIANCETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DISHWASHER",
                "ELECTRICCOOKER",
                "FREESTANDINGELECTRICHEATER",
                "FREESTANDINGFAN",
                "FREESTANDINGWATERHEATER",
                "FREESTANDINGWATERCOOLER",
                "FREEZER",
                "FRIDGE_FREEZER",
                "HANDDRYER",
                "KITCHENMACHINE",
                "MICROWAVE",
                "PHOTOCOPIER",
                "REFRIGERATOR",
                "TUMBLEDRYER",
                "VENDINGMACHINE",
                "WASHINGMACHINE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICDISTRIBUTIONBOARDTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONSUMERUNIT",
                "DISTRIBUTIONBOARD",
                "MOTORCONTROLCENTRE",
                "SWITCHBOARD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICFLOWSTORAGEDEVICETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BATTERY",
                "CAPACITORBANK",
                "HARMONICFILTER",
                "INDUCTORBANK",
                "UPS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICGENERATORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CHP",
                "ENGINEGENERATOR",
                "STANDALONE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICMOTORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DC",
                "INDUCTION",
                "POLYPHASE",
                "RELUCTANCESYNCHRONOUS",
                "SYNCHRONOUS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELECTRICTIMECONTROLTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "TIMECLOCK",
                "TIMEDELAY",
                "RELAY",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCELEMENTASSEMBLYTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ACCESSORY_ASSEMBLY",
                "ARCH",
                "BEAM_GRID",
                "BRACED_FRAME",
                "GIRDER",
                "REINFORCEMENT_UNIT",
                "RIGID_FRAME",
                "SLAB_FIELD",
                "TRUSS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCENGINETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "EXTERNALCOMBUSTION",
                "INTERNALCOMBUSTION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCEVAPORATIVECOOLERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DIRECTEVAPORATIVERANDOMMEDIAAIRCOOLER",
                "DIRECTEVAPORATIVERIGIDMEDIAAIRCOOLER",
                "DIRECTEVAPORATIVESLINGERSPACKAGEDAIRCOOLER",
                "DIRECTEVAPORATIVEPACKAGEDROTARYAIRCOOLER",
                "DIRECTEVAPORATIVEAIRWASHER",
                "INDIRECTEVAPORATIVEPACKAGEAIRCOOLER",
                "INDIRECTEVAPORATIVEWETCOIL",
                "INDIRECTEVAPORATIVECOOLINGTOWERORCOILCOOLER",
                "INDIRECTDIRECTCOMBINATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCEVAPORATORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "DIRECTEXPANSION",
                "DIRECTEXPANSIONSHELLANDTUBE",
                "DIRECTEXPANSIONTUBEINTUBE",
                "DIRECTEXPANSIONBRAZEDPLATE",
                "FLOODEDSHELLANDTUBE",
                "SHELLANDCOIL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFANTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CENTRIFUGALFORWARDCURVED",
                "CENTRIFUGALRADIAL",
                "CENTRIFUGALBACKWARDINCLINEDCURVED",
                "CENTRIFUGALAIRFOIL",
                "TUBEAXIAL",
                "VANEAXIAL",
                "PROPELLORAXIAL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFASTENERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["GLUE", "MORTAR", "WELD", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFILTERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRPARTICLEFILTER",
                "COMPRESSEDAIRFILTER",
                "ODORFILTER",
                "OILFILTER",
                "STRAINER",
                "WATERFILTER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFIRESUPPRESSIONTERMINALTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BREECHINGINLET",
                "FIREHYDRANT",
                "HOSEREEL",
                "SPRINKLER",
                "SPRINKLERDEFLECTOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFLOWINSTRUMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "PRESSUREGAUGE",
                "THERMOMETER",
                "AMMETER",
                "FREQUENCYMETER",
                "POWERFACTORMETER",
                "PHASEANGLEMETER",
                "VOLTMETER_PEAK",
                "VOLTMETER_RMS",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFLOWMETERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ENERGYMETER",
                "GASMETER",
                "OILMETER",
                "WATERMETER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCFOOTINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CAISSON_FOUNDATION",
                "FOOTING_BEAM",
                "PAD_FOOTING",
                "PILE_CAP",
                "STRIP_FOOTING",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    Ifc4Entity {
        name: "IFCFURNISHINGELEMENTTYPE",
        attributes: &[],
        predefined_type: None,
    },
    // `AssemblyPlace`, `PredefinedType`
    Ifc4Entity {
        name: "IFCFURNITURETYPE",
        attributes: &[Attribute::Notdefined, Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 1,
            members: &[
                "CHAIR",
                "TABLE",
                "DESK",
                "BED",
                "FILECABINET",
                "SHELF",
                "SOFA",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCGEOGRAPHICELEMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["TERRAIN", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCHEATEXCHANGERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["PLATE", "SHELLANDTUBE", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCHUMIDIFIERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STEAMINJECTION",
                "ADIABATICAIRWASHER",
                "ADIABATICPAN",
                "ADIABATICWETTEDELEMENT",
                "ADIABATICATOMIZING",
                "ADIABATICULTRASONIC",
                "ADIABATICRIGIDMEDIA",
                "ADIABATICCOMPRESSEDAIRNOZZLE",
                "ASSISTEDELECTRIC",
                "ASSISTEDNATURALGAS",
                "ASSISTEDPROPANE",
                "ASSISTEDBUTANE",
                "ASSISTEDSTEAM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCINTERCEPTORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CYCLONIC",
                "GREASE",
                "OIL",
                "PETROL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCJUNCTIONBOXTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["DATA", "POWER", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCLAMPTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COMPACTFLUORESCENT",
                "FLUORESCENT",
                "HALOGEN",
                "HIGHPRESSUREMERCURY",
                "HIGHPRESSURESODIUM",
                "LED",
                "METALHALIDE",
                "OLED",
                "TUNGSTENFILAMENT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCLIGHTFIXTURETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "POINTSOURCE",
                "DIRECTIONSOURCE",
                "SECURITYLIGHTING",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `NominalDiameter`, `NominalLength`
    Ifc4Entity {
        name: "IFCMECHANICALFASTENERTYPE",
        attributes: &[
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ANCHORBOLT",
                "BOLT",
                "DOWEL",
                "NAIL",
                "NAILPLATE",
                "RIVET",
                "SCREW",
                "SHEARCONNECTOR",
                "STAPLE",
                "STUDSHEARCONNECTOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMEDICALDEVICETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRSTATION",
                "FEEDAIRUNIT",
                "OXYGENGENERATOR",
                "OXYGENPLANT",
                "VACUUMSTATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMEMBERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BRACE",
                "CHORD",
                "COLLAR",
                "MEMBER",
                "MULLION",
                "PLATE",
                "POST",
                "PURLIN",
                "RAFTER",
                "STRINGER",
                "STRUT",
                "STUD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCMOTORCONNECTIONTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BELTDRIVE",
                "COUPLING",
                "DIRECTDRIVE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCOUTLETTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AUDIOVISUALOUTLET",
                "COMMUNICATIONSOUTLET",
                "POWEROUTLET",
                "DATAOUTLET",
                "TELEPHONEOUTLET",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPILETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BORED",
                "DRIVEN",
                "JETGROUTING",
                "COHESION",
                "FRICTION",
                "SUPPORT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPIPEFITTINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BEND",
                "CONNECTOR",
                "ENTRY",
                "EXIT",
                "JUNCTION",
                "OBSTRUCTION",
                "TRANSITION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPIPESEGMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CULVERT",
                "FLEXIBLESEGMENT",
                "RIGIDSEGMENT",
                "GUTTER",
                "SPOOL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPLATETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["CURTAIN_PANEL", "SHEET", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPROTECTIVEDEVICETRIPPINGUNITTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ELECTRONIC",
                "ELECTROMAGNETIC",
                "RESIDUALCURRENT",
                "THERMAL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPROTECTIVEDEVICETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CIRCUITBREAKER",
                "EARTHLEAKAGECIRCUITBREAKER",
                "EARTHINGSWITCH",
                "FUSEDISCONNECTOR",
                "RESIDUALCURRENTCIRCUITBREAKER",
                "RESIDUALCURRENTSWITCH",
                "VARISTOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCPUMPTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CIRCULATOR",
                "ENDSUCTION",
                "SPLITCASE",
                "SUBMERSIBLEPUMP",
                "SUMPPUMP",
                "VERTICALINLINE",
                "VERTICALTURBINE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCRAILINGTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "HANDRAIL",
                "GUARDRAIL",
                "BALUSTRADE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCRAMPFLIGHTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["STRAIGHT", "SPIRAL", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCRAMPTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STRAIGHT_RUN_RAMP",
                "TWO_STRAIGHT_RUN_RAMP",
                "QUARTER_TURN_RAMP",
                "TWO_QUARTER_TURN_RAMP",
                "HALF_TURN_RAMP",
                "SPIRAL_RAMP",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `NominalDiameter`, `CrossSectionArea`, `BarLength`, `BarSurface`, `BendingShapeCode`, `BendingParameters`
    Ifc4Entity {
        name: "IFCREINFORCINGBARTYPE",
        attributes: &[
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ANCHORING",
                "EDGE",
                "LIGATURE",
                "MAIN",
                "PUNCHING",
                "RING",
                "SHEAR",
                "STUD",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `MeshLength`, `MeshWidth`, `LongitudinalBarNominalDiameter`, `TransverseBarNominalDiameter`, `LongitudinalBarCrossSectionArea`, `TransverseBarCrossSectionArea`, `LongitudinalBarSpacing`, `TransverseBarSpacing`, `BendingShapeCode`, `BendingParameters`
    Ifc4Entity {
        name: "IFCREINFORCINGMESHTYPE",
        attributes: &[
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCROOFTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLAT_ROOF",
                "SHED_ROOF",
                "GABLE_ROOF",
                "HIP_ROOF",
                "HIPPED_GABLE_ROOF",
                "GAMBREL_ROOF",
                "MANSARD_ROOF",
                "BARREL_ROOF",
                "RAINBOW_ROOF",
                "BUTTERFLY_ROOF",
                "PAVILION_ROOF",
                "DOME_ROOF",
                "FREEFORM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSANITARYTERMINALTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BATH",
                "BIDET",
                "CISTERN",
                "SHOWER",
                "SINK",
                "SANITARYFOUNTAIN",
                "TOILETPAN",
                "URINAL",
                "WASHHANDBASIN",
                "WCSEAT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSENSORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COSENSOR",
                "CO2SENSOR",
                "CONDUCTANCESENSOR",
                "CONTACTSENSOR",
                "FIRESENSOR",
                "FLOWSENSOR",
                "FROSTSENSOR",
                "GASSENSOR",
                "HEATSENSOR",
                "HUMIDITYSENSOR",
                "IDENTIFIERSENSOR",
                "IONCONCENTRATIONSENSOR",
                "LEVELSENSOR",
                "LIGHTSENSOR",
                "MOISTURESENSOR",
                "MOVEMENTSENSOR",
                "PHSENSOR",
                "PRESSURESENSOR",
                "RADIATIONSENSOR",
                "RADIOACTIVITYSENSOR",
                "SMOKESENSOR",
                "SOUNDSENSOR",
                "TEMPERATURESENSOR",
                "WINDSENSOR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSHADINGDEVICETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["JALOUSIE", "SHUTTER", "AWNING", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSLABTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOOR",
                "ROOF",
                "LANDING",
                "BASESLAB",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSOLARDEVICETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["SOLARCOLLECTOR", "SOLARPANEL", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSPACEHEATERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["CONVECTOR", "RADIATOR", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSTACKTERMINALTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BIRDCAGE",
                "COWL",
                "RAINWATERHOPPER",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSTAIRFLIGHTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STRAIGHT",
                "WINDER",
                "SPIRAL",
                "CURVED",
                "FREEFORM",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSTAIRTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "STRAIGHT_RUN_STAIR",
                "TWO_STRAIGHT_RUN_STAIR",
                "QUARTER_WINDING_STAIR",
                "QUARTER_TURN_STAIR",
                "HALF_WINDING_STAIR",
                "HALF_TURN_STAIR",
                "TWO_QUARTER_WINDING_STAIR",
                "TWO_QUARTER_TURN_STAIR",
                "THREE_QUARTER_WINDING_STAIR",
                "THREE_QUARTER_TURN_STAIR",
                "SPIRAL_STAIR",
                "DOUBLE_RETURN_STAIR",
                "CURVED_RUN_STAIR",
                "TWO_CURVED_RUN_STAIR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSWITCHINGDEVICETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONTACTOR",
                "DIMMERSWITCH",
                "EMERGENCYSTOP",
                "KEYPAD",
                "MOMENTARYSWITCH",
                "SELECTORSWITCH",
                "STARTER",
                "SWITCHDISCONNECTOR",
                "TOGGLESWITCH",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCSYSTEMFURNITUREELEMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["PANEL", "WORKSURFACE", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTANKTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BASIN",
                "BREAKPRESSURE",
                "EXPANSION",
                "FEEDANDEXPANSION",
                "PRESSUREVESSEL",
                "STORAGE",
                "VESSEL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTENDONANCHORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "COUPLER",
                "FIXED_END",
                "TENSIONING_END",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `NominalDiameter`, `CrossSectionArea`, `SheathDiameter`
    Ifc4Entity {
        name: "IFCTENDONTYPE",
        attributes: &[
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "BAR",
                "COATED",
                "STRAND",
                "WIRE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTRANSFORMERTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CURRENT",
                "FREQUENCY",
                "INVERTER",
                "RECTIFIER",
                "VOLTAGE",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTRANSPORTELEMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ELEVATOR",
                "ESCALATOR",
                "MOVINGWALKWAY",
                "CRANEWAY",
                "LIFTINGGEAR",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCTUBEBUNDLETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["FINNED", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCUNITARYCONTROLELEMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "ALARMPANEL",
                "CONTROLPANEL",
                "GASDETECTIONPANEL",
                "INDICATORPANEL",
                "MIMICPANEL",
                "HUMIDISTAT",
                "THERMOSTAT",
                "WEATHERSTATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCUNITARYEQUIPMENTTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRHANDLER",
                "AIRCONDITIONINGUNIT",
                "DEHUMIDIFIER",
                "SPLITSYSTEM",
                "ROOFTOPUNIT",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCVALVETYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "AIRRELEASE",
                "ANTIVACUUM",
                "CHANGEOVER",
                "CHECK",
                "COMMISSIONING",
                "DIVERTING",
                "DRAWOFFCOCK",
                "DOUBLECHECK",
                "DOUBLEREGULATING",
                "FAUCET",
                "FLUSHING",
                "GASCOCK",
                "GASTAP",
                "ISOLATING",
                "MIXING",
                "PRESSUREREDUCING",
                "PRESSURERELIEF",
                "REGULATING",
                "SAFETYCUTOFF",
                "STEAMTRAP",
                "STOPCOCK",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCVIBRATIONISOLATORTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &["COMPRESSION", "SPRING", "USERDEFINED", "NOTDEFINED"],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCWALLTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "MOVABLE",
                "PARAPET",
                "PARTITIONING",
                "PLUMBINGWALL",
                "SHEAR",
                "SOLIDWALL",
                "STANDARD",
                "POLYGONAL",
                "ELEMENTEDWALL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`
    Ifc4Entity {
        name: "IFCWASTETERMINALTYPE",
        attributes: &[Attribute::Notdefined],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "FLOORTRAP",
                "FLOORWASTE",
                "GULLYSUMP",
                "GULLYTRAP",
                "ROOFDRAIN",
                "WASTEDISPOSALUNIT",
                "WASTETRAP",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `PartitioningType`, `ParameterTakesPrecedence`, `UserDefinedPartitioningType`
    Ifc4Entity {
        name: "IFCWINDOWTYPE",
        attributes: &[
            Attribute::Notdefined,
            Attribute::Notdefined,
            Attribute::Optional,
            Attribute::Optional,
        ],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "WINDOW",
                "SKYLIGHT",
                "LIGHTDOME",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
];

/// Every instantiable `IfcSpatialElementType`, past `ElementType`.
pub(crate) const IFC4_SPATIAL_ELEMENT_TYPES: &[Ifc4Entity] = &[
    // `PredefinedType`, `LongName`
    Ifc4Entity {
        name: "IFCSPACETYPE",
        attributes: &[Attribute::Notdefined, Attribute::Optional],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "SPACE",
                "PARKING",
                "GFA",
                "INTERNAL",
                "EXTERNAL",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
    // `PredefinedType`, `LongName`
    Ifc4Entity {
        name: "IFCSPATIALZONETYPE",
        attributes: &[Attribute::Notdefined, Attribute::Optional],
        predefined_type: Some(PredefinedType {
            index: 0,
            members: &[
                "CONSTRUCTION",
                "FIRESAFETY",
                "LIGHTING",
                "OCCUPANCY",
                "SECURITY",
                "THERMAL",
                "TRANSPORT",
                "VENTILATION",
                "USERDEFINED",
                "NOTDEFINED",
            ],
        }),
    },
];

/// IFC's own `Pset_..Common` for an entity, where the property set
/// templates define one that holds `Reference`.
pub(crate) const IFC4_COMMON_PROPERTY_SETS: &[(&str, &str)] = &[
    ("IFCACTUATOR", "Pset_ActuatorTypeCommon"),
    ("IFCAIRTERMINAL", "Pset_AirTerminalTypeCommon"),
    ("IFCAIRTERMINALBOX", "Pset_AirTerminalBoxTypeCommon"),
    (
        "IFCAIRTOAIRHEATRECOVERY",
        "Pset_AirToAirHeatRecoveryTypeCommon",
    ),
    ("IFCALARM", "Pset_AlarmTypeCommon"),
    (
        "IFCAUDIOVISUALAPPLIANCE",
        "Pset_AudioVisualApplianceTypeCommon",
    ),
    ("IFCBEAM", "Pset_BeamCommon"),
    ("IFCBOILER", "Pset_BoilerTypeCommon"),
    ("IFCBUILDING", "Pset_BuildingCommon"),
    ("IFCBUILDINGELEMENTPROXY", "Pset_BuildingElementProxyCommon"),
    ("IFCBUILDINGSTOREY", "Pset_BuildingStoreyCommon"),
    ("IFCBUILDINGSYSTEM", "Pset_BuildingSystemCommon"),
    ("IFCBURNER", "Pset_BurnerTypeCommon"),
    (
        "IFCCABLECARRIERFITTING",
        "Pset_CableCarrierFittingTypeCommon",
    ),
    (
        "IFCCABLECARRIERSEGMENT",
        "Pset_CableCarrierSegmentTypeCommon",
    ),
    ("IFCCABLEFITTING", "Pset_CableFittingTypeCommon"),
    ("IFCCABLESEGMENT", "Pset_CableSegmentTypeCommon"),
    ("IFCCHILLER", "Pset_ChillerTypeCommon"),
    ("IFCCHIMNEY", "Pset_ChimneyCommon"),
    ("IFCCIVILELEMENT", "Pset_CivilElementCommon"),
    ("IFCCOIL", "Pset_CoilTypeCommon"),
    ("IFCCOLUMN", "Pset_ColumnCommon"),
    (
        "IFCCOMMUNICATIONSAPPLIANCE",
        "Pset_CommunicationsApplianceTypeCommon",
    ),
    ("IFCCOMPRESSOR", "Pset_CompressorTypeCommon"),
    ("IFCCONDENSER", "Pset_CondenserTypeCommon"),
    ("IFCCONTROLLER", "Pset_ControllerTypeCommon"),
    ("IFCCOOLEDBEAM", "Pset_CooledBeamTypeCommon"),
    ("IFCCOOLINGTOWER", "Pset_CoolingTowerTypeCommon"),
    ("IFCCOVERING", "Pset_CoveringCommon"),
    ("IFCCURTAINWALL", "Pset_CurtainWallCommon"),
    ("IFCDAMPER", "Pset_DamperTypeCommon"),
    (
        "IFCDISTRIBUTIONCHAMBERELEMENT",
        "Pset_DistributionChamberElementCommon",
    ),
    ("IFCDISTRIBUTIONSYSTEM", "Pset_DistributionSystemCommon"),
    ("IFCDOOR", "Pset_DoorCommon"),
    ("IFCDUCTFITTING", "Pset_DuctFittingTypeCommon"),
    ("IFCDUCTSEGMENT", "Pset_DuctSegmentTypeCommon"),
    ("IFCDUCTSILENCER", "Pset_DuctSilencerTypeCommon"),
    ("IFCELECTRICAPPLIANCE", "Pset_ElectricApplianceTypeCommon"),
    (
        "IFCELECTRICDISTRIBUTIONBOARD",
        "Pset_ElectricDistributionBoardTypeCommon",
    ),
    (
        "IFCELECTRICFLOWSTORAGEDEVICE",
        "Pset_ElectricFlowStorageDeviceTypeCommon",
    ),
    ("IFCELECTRICGENERATOR", "Pset_ElectricGeneratorTypeCommon"),
    ("IFCELECTRICMOTOR", "Pset_ElectricMotorTypeCommon"),
    (
        "IFCELECTRICTIMECONTROL",
        "Pset_ElectricTimeControlTypeCommon",
    ),
    ("IFCELEMENTASSEMBLY", "Pset_ElementAssemblyCommon"),
    ("IFCELEMENTCOMPONENT", "Pset_ElementComponentCommon"),
    ("IFCENGINE", "Pset_EngineTypeCommon"),
    ("IFCEVAPORATIVECOOLER", "Pset_EvaporativeCoolerTypeCommon"),
    ("IFCEVAPORATOR", "Pset_EvaporatorTypeCommon"),
    ("IFCFAN", "Pset_FanTypeCommon"),
    ("IFCFILTER", "Pset_FilterTypeCommon"),
    (
        "IFCFIRESUPPRESSIONTERMINAL",
        "Pset_FireSuppressionTerminalTypeCommon",
    ),
    ("IFCFLOWINSTRUMENT", "Pset_FlowInstrumentTypeCommon"),
    ("IFCFLOWMETER", "Pset_FlowMeterTypeCommon"),
    ("IFCFOOTING", "Pset_FootingCommon"),
    ("IFCFURNITURE", "Pset_FurnitureTypeCommon"),
    ("IFCHEATEXCHANGER", "Pset_HeatExchangerTypeCommon"),
    ("IFCHUMIDIFIER", "Pset_HumidifierTypeCommon"),
    ("IFCINTERCEPTOR", "Pset_InterceptorTypeCommon"),
    ("IFCJUNCTIONBOX", "Pset_JunctionBoxTypeCommon"),
    ("IFCLAMP", "Pset_LampTypeCommon"),
    ("IFCLIGHTFIXTURE", "Pset_LightFixtureTypeCommon"),
    ("IFCMEDICALDEVICE", "Pset_MedicalDeviceTypeCommon"),
    ("IFCMEMBER", "Pset_MemberCommon"),
    ("IFCMOTORCONNECTION", "Pset_MotorConnectionTypeCommon"),
    ("IFCOPENINGELEMENT", "Pset_OpeningElementCommon"),
    ("IFCOUTLET", "Pset_OutletTypeCommon"),
    ("IFCPILE", "Pset_PileCommon"),
    ("IFCPIPEFITTING", "Pset_PipeFittingTypeCommon"),
    ("IFCPIPESEGMENT", "Pset_PipeSegmentTypeCommon"),
    ("IFCPLATE", "Pset_PlateCommon"),
    ("IFCPROTECTIVEDEVICE", "Pset_ProtectiveDeviceTypeCommon"),
    (
        "IFCPROTECTIVEDEVICETRIPPINGUNIT",
        "Pset_ProtectiveDeviceTrippingUnitTypeCommon",
    ),
    ("IFCPUMP", "Pset_PumpTypeCommon"),
    ("IFCRAILING", "Pset_RailingCommon"),
    ("IFCRAMP", "Pset_RampCommon"),
    ("IFCRAMPFLIGHT", "Pset_RampFlightCommon"),
    ("IFCREINFORCINGBAR", "Pset_ReinforcingBarCommon"),
    ("IFCREINFORCINGMESH", "Pset_ReinforcingMeshCommon"),
    ("IFCROOF", "Pset_RoofCommon"),
    ("IFCSANITARYTERMINAL", "Pset_SanitaryTerminalTypeCommon"),
    ("IFCSENSOR", "Pset_SensorTypeCommon"),
    ("IFCSHADINGDEVICE", "Pset_ShadingDeviceCommon"),
    ("IFCSITE", "Pset_SiteCommon"),
    ("IFCSLAB", "Pset_SlabCommon"),
    ("IFCSOLARDEVICE", "Pset_SolarDeviceTypeCommon"),
    ("IFCSPACE", "Pset_SpaceCommon"),
    ("IFCSPACEHEATER", "Pset_SpaceHeaterTypeCommon"),
    ("IFCSPATIALZONE", "Pset_SpatialZoneCommon"),
    ("IFCSTACKTERMINAL", "Pset_StackTerminalTypeCommon"),
    ("IFCSTAIR", "Pset_StairCommon"),
    ("IFCSTAIRFLIGHT", "Pset_StairFlightCommon"),
    ("IFCSWITCHINGDEVICE", "Pset_SwitchingDeviceTypeCommon"),
    ("IFCTANK", "Pset_TankTypeCommon"),
    ("IFCTENDON", "Pset_TendonCommon"),
    ("IFCTENDONANCHOR", "Pset_TendonAnchorCommon"),
    ("IFCTRANSFORMER", "Pset_TransformerTypeCommon"),
    ("IFCTRANSPORTELEMENT", "Pset_TransportElementCommon"),
    ("IFCTUBEBUNDLE", "Pset_TubeBundleTypeCommon"),
    (
        "IFCUNITARYCONTROLELEMENT",
        "Pset_UnitaryControlElementTypeCommon",
    ),
    ("IFCUNITARYEQUIPMENT", "Pset_UnitaryEquipmentTypeCommon"),
    ("IFCVALVE", "Pset_ValveTypeCommon"),
    ("IFCVIBRATIONISOLATOR", "Pset_VibrationIsolatorTypeCommon"),
    ("IFCWALL", "Pset_WallCommon"),
    ("IFCWASTETERMINAL", "Pset_WasteTerminalTypeCommon"),
    ("IFCWINDOW", "Pset_WindowCommon"),
    ("IFCZONE", "Pset_ZoneCommon"),
];

/// IFC's own base quantity set for an entity, and the quantities it
/// holds. The exporter writes only the ones it has measured.
pub(crate) const IFC4_BASE_QUANTITY_SETS: &[(&str, &str, &[&str])] = &[
    (
        "IFCACTUATOR",
        "Qto_ActuatorBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCAIRTERMINAL",
        "Qto_AirTerminalBaseQuantities",
        &["GrossWeight", "Perimeter", "TotalSurfaceArea"],
    ),
    (
        "IFCAIRTERMINALBOX",
        "Qto_AirTerminalBoxTypeBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCAIRTOAIRHEATRECOVERY",
        "Qto_AirToAirHeatRecoveryBaseQuantities",
        &["GrossWeight"],
    ),
    ("IFCALARM", "Qto_AlarmBaseQuantities", &["GrossWeight"]),
    (
        "IFCAUDIOVISUALAPPLIANCE",
        "Qto_AudioVisualApplianceBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCBEAM",
        "Qto_BeamBaseQuantities",
        &[
            "Length",
            "CrossSectionArea",
            "OuterSurfaceArea",
            "GrossSurfaceArea",
            "NetSurfaceArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCBOILER",
        "Qto_BoilerBaseQuantities",
        &["GrossWeight", "NetWeight", "TotalSurfaceArea"],
    ),
    (
        "IFCBUILDING",
        "Qto_BuildingBaseQuantities",
        &[
            "Height",
            "EavesHeight",
            "FootprintArea",
            "GrossFloorArea",
            "NetFloorArea",
            "GrossVolume",
            "NetVolume",
        ],
    ),
    (
        "IFCBUILDINGELEMENTPROXY",
        "Qto_BuildingElementProxyQuantities",
        &["NetSurfaceArea", "NetVolume"],
    ),
    (
        "IFCBUILDINGSTOREY",
        "Qto_BuildingStoreyBaseQuantities",
        &[
            "GrossHeight",
            "NetHeigtht",
            "GrossPerimeter",
            "GrossFloorArea",
            "NetFloorArea",
            "GrossVolume",
            "NetVolume",
        ],
    ),
    ("IFCBURNER", "Qto_BurnerBaseQuantities", &["GrossWeight"]),
    (
        "IFCCABLECARRIERFITTING",
        "Qto_CableCarrierFittingBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCABLECARRIERSEGMENT",
        "Qto_CableCarrierSegmentBaseQuantities",
        &[
            "GrossWeight",
            "Length",
            "CrossSectionArea",
            "OuterSurfaceArea",
        ],
    ),
    (
        "IFCCABLEFITTING",
        "Qto_CableFittingBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCABLESEGMENT",
        "Qto_CableSegmentBaseQuantities",
        &[
            "GrossWeight",
            "Length",
            "CrossSectionArea",
            "OuterSurfaceArea",
        ],
    ),
    ("IFCCHILLER", "Qto_ChillerBaseQuantities", &["GrossWeight"]),
    ("IFCCHIMNEY", "Qto_ChimneyBaseQuantities", &["Length"]),
    ("IFCCOIL", "Qto_CoilBaseQuantities", &["GrossWeight"]),
    (
        "IFCCOLUMN",
        "Qto_ColumnBaseQuantities",
        &[
            "Length",
            "CrossSectionArea",
            "OuterSurfaceArea",
            "GrossSurfaceArea",
            "NetSurfaceArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCCOMMUNICATIONSAPPLIANCE",
        "Qto_CommunicationsApplianceBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCOMPRESSOR",
        "Qto_CompressorBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCONDENSER",
        "Qto_CondenserBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCONSTRUCTIONEQUIPMENTRESOURCE",
        "Qto_ConstructionEquipmentResourceBaseQuantities",
        &["UsageTime", "OperatingTime"],
    ),
    (
        "IFCCONSTRUCTIONMATERIALRESOURCE",
        "Qto_ConstructionMaterialResourceBaseQuantities",
        &["GrossVolume", "NetVolume", "GrossWeight", "NetWeight"],
    ),
    (
        "IFCCONTROLLER",
        "Qto_ControllerBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCOOLEDBEAM",
        "Qto_CooledBeamBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCOOLINGTOWER",
        "Qto_CoolingTowerBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCCOVERING",
        "Qto_CoveringBaseQuantities",
        &["Width", "GrossArea", "NetArea"],
    ),
    (
        "IFCCURTAINWALL",
        "Qto_CurtainWallQuantities",
        &["Length", "Height", "Width", "GrossSideArea", "NetSideArea"],
    ),
    ("IFCDAMPER", "Qto_DamperBaseQuantities", &["GrossWeight"]),
    (
        "IFCDISTRIBUTIONCHAMBERELEMENT",
        "Qto_DistributionChamberElementBaseQuantities",
        &[
            "GrossSurfaceArea",
            "NetSurfaceArea",
            "GrossVolume",
            "NetVolume",
        ],
    ),
    (
        "IFCDOOR",
        "Qto_DoorBaseQuantities",
        &["Width", "Height", "Perimeter", "Area"],
    ),
    (
        "IFCDUCTFITTING",
        "Qto_DuctFittingBaseQuantities",
        &[
            "Length",
            "GrossCrossSectionArea",
            "NetCrossSectionArea",
            "OuterSurfaceArea",
            "GrossWeight",
        ],
    ),
    (
        "IFCDUCTSEGMENT",
        "Qto_DuctSegmentBaseQuantities",
        &[
            "Length",
            "GrossCrossSectionArea",
            "NetCrossSectionArea",
            "OuterSurfaceArea",
            "GrossWeight",
        ],
    ),
    (
        "IFCDUCTSILENCER",
        "Qto_DuctSilencerBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCELECTRICAPPLIANCE",
        "Qto_ElectricApplianceBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCELECTRICDISTRIBUTIONBOARD",
        "Qto_ElectricDistributionBoardBaseQuantities",
        &["GrossWeight", "NumberOfCircuits"],
    ),
    (
        "IFCELECTRICFLOWSTORAGEDEVICE",
        "Qto_ElectricFlowStorageDeviceBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCELECTRICGENERATOR",
        "Qto_ElectricGeneratorBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCELECTRICMOTOR",
        "Qto_ElectricMotorBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCELECTRICTIMECONTROL",
        "Qto_ElectricTimeControlBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCEVAPORATIVECOOLER",
        "Qto_EvaporativeCoolerBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCEVAPORATOR",
        "Qto_EvaporatorBaseQuantities",
        &["GrossWeight"],
    ),
    ("IFCFAN", "Qto_FanBaseQuantities", &["GrossWeight"]),
    ("IFCFILTER", "Qto_FilterBaseQuantities", &["GrossWeight"]),
    (
        "IFCFIRESUPPRESSIONTERMINAL",
        "Qto_FireSuppressionTerminalBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCFLOWINSTRUMENT",
        "Qto_FlowInstrumentBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCFLOWMETER",
        "Qto_FlowMeterBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCFOOTING",
        "Qto_FootingBaseQuantities",
        &[
            "Length",
            "Width",
            "Height",
            "CrossSectionArea",
            "OuterSurfaceArea",
            "GrossSurfaceArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCHEATEXCHANGER",
        "Qto_HeatExchangerBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCHUMIDIFIER",
        "Qto_HumidifierBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCINTERCEPTOR",
        "Qto_InterceptorBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCJUNCTIONBOX",
        "Qto_JunctionBoxBaseQuantities",
        &["GrossWeight", "NumberOfGangs"],
    ),
    (
        "IFCLABORRESOURCE",
        "Qto_LaborResourceBaseQuantities",
        &["StandardWork", "OvertimeWork"],
    ),
    ("IFCLAMP", "Qto_LampBaseQuantities", &["GrossWeight"]),
    (
        "IFCLIGHTFIXTURE",
        "Qto_LightFixtureBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCMEMBER",
        "Qto_MemberBaseQuantities",
        &[
            "Length",
            "CrossSectionArea",
            "OuterSurfaceArea",
            "GrossSurfaceArea",
            "NetSurfaceArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCMOTORCONNECTION",
        "Qto_MotorConnectionBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCOPENINGELEMENT",
        "Qto_OpeningElementBaseQuantities",
        &["Width", "Height", "Depth", "Area", "Volume"],
    ),
    ("IFCOUTLET", "Qto_OutletBaseQuantities", &["GrossWeight"]),
    (
        "IFCPILE",
        "Qto_PileBaseQuantities",
        &[
            "Length",
            "CrossSectionArea",
            "OuterSurfaceArea",
            "GrossSurfaceArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCPIPEFITTING",
        "Qto_PipeFittingBaseQuantities",
        &[
            "Length",
            "GrossCrossSectionArea",
            "NetCrossSectionArea",
            "OuterSurfaceArea",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCPIPESEGMENT",
        "Qto_PipeSegmentBaseQuantities",
        &[
            "Length",
            "GrossCrossSectionArea",
            "NetCrossSectionArea",
            "OuterSurfaceArea",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCPLATE",
        "Qto_PlateBaseQuantities",
        &[
            "Width",
            "Perimeter",
            "GrossArea",
            "NetArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCPROJECTIONELEMENT",
        "Qto_ProjectionElementBaseQuantities",
        &["Area", "Volume"],
    ),
    (
        "IFCPROTECTIVEDEVICE",
        "Qto_ProtectiveDeviceBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCPROTECTIVEDEVICETRIPPINGUNIT",
        "Qto_ProtectiveDeviceTrippingUnitBaseQuantities",
        &["GrossWeight"],
    ),
    ("IFCPUMP", "Qto_PumpBaseQuantities", &["GrossWeight"]),
    ("IFCRAILING", "Qto_RailingBaseQuantities", &["Length"]),
    (
        "IFCRAMPFLIGHT",
        "Qto_RampFlightBaseQuantities",
        &[
            "Length",
            "Width",
            "GrossArea",
            "NetArea",
            "GrossVolume",
            "NetVolume",
        ],
    ),
    (
        "IFCREINFORCINGELEMENT",
        "Qto_ReinforcingElementBaseQuantities",
        &["Count", "Length", "Weight"],
    ),
    (
        "IFCROOF",
        "Qto_RoofBaseQuantities",
        &["GrossArea", "NetArea", "ProjectedArea"],
    ),
    (
        "IFCSANITARYTERMINAL",
        "Qto_SanitaryTerminalBaseQuantities",
        &["GrossWeight"],
    ),
    ("IFCSENSOR", "Qto_SensorBaseQuantities", &["GrossWeight"]),
    (
        "IFCSITE",
        "Qto_SiteBaseQuantities",
        &["GrossPerimeter", "GrossArea"],
    ),
    (
        "IFCSLAB",
        "Qto_SlabBaseQuantities",
        &[
            "Width",
            "Length",
            "Depth",
            "Perimeter",
            "GrossArea",
            "NetArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCSOLARDEVICE",
        "Qto_SolarDeviceBaseQuantities",
        &["GrossWeight", "GrossArea"],
    ),
    (
        "IFCSPACE",
        "Qto_SpaceBaseQuantities",
        &[
            "Height",
            "FinishCeilingHeight",
            "FinishFloorHeight",
            "GrossPerimeter",
            "NetPerimeter",
            "GrossFloorArea",
            "NetFloorArea",
            "GrossWallArea",
            "NetWallArea",
            "GrossCeilingArea",
            "NetCeilingArea",
            "GrossVolume",
            "NetVolume",
        ],
    ),
    (
        "IFCSPACEHEATER",
        "Qto_SpaceHeaterBaseQuantities",
        &["Length", "GrossWeight", "NetWeight"],
    ),
    (
        "IFCSTACKTERMINAL",
        "Qto_StackTerminalBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCSTAIRFLIGHT",
        "Qto_StairFlightBaseQuantities",
        &["Length", "GrossVolume", "NetVolume"],
    ),
    (
        "IFCSWITCHINGDEVICE",
        "Qto_SwitchingDeviceBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCTANK",
        "Qto_TankBaseQuantities",
        &["GrossWeight", "NetWeight", "TotalSurfaceArea"],
    ),
    (
        "IFCTRANSFORMER",
        "Qto_TransformerBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCTUBEBUNDLE",
        "Qto_TubeBundleBaseQuantities",
        &["GrossWeight", "NetWeight"],
    ),
    (
        "IFCUNITARYCONTROLELEMENT",
        "Qto_UnitaryControlElementBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCUNITARYEQUIPMENT",
        "Qto_UnitaryEquipmentBaseQuantities",
        &["GrossWeight"],
    ),
    ("IFCVALVE", "Qto_ValveBaseQuantities", &["GrossWeight"]),
    (
        "IFCVIBRATIONISOLATOR",
        "Qto_VibrationIsolatorBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCWALL",
        "Qto_WallBaseQuantities",
        &[
            "Length",
            "Width",
            "Height",
            "GrossFootprintArea",
            "NetFootprintArea",
            "GrossSideArea",
            "NetSideArea",
            "GrossVolume",
            "NetVolume",
            "GrossWeight",
            "NetWeight",
        ],
    ),
    (
        "IFCWASTETERMINAL",
        "Qto_WasteTerminalBaseQuantities",
        &["GrossWeight"],
    ),
    (
        "IFCWINDOW",
        "Qto_WindowBaseQuantities",
        &["Width", "Height", "Perimeter", "Area"],
    ),
];

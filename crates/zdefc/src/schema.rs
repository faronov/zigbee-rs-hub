//! JSON Schema of the source format (`schema/zdef-v1.schema.json`) and a
//! validator for the JSON Schema 2020-12 subset it uses.
//!
//! The schema is generated from the `zdef` tables (platforms, device
//! classes, units, ZCL types, part names, features), so it cannot drift from
//! the encoder; a test checks the published file against the generator.
//! Keywords used and implemented: `type`, `enum`, `const`, `properties`,
//! `required`, `additionalProperties`, `propertyNames`, `minProperties`,
//! `items`, `prefixItems`, `minItems`, `maxItems`, `minimum`, `maximum`,
//! `minLength`, `maxLength`, `pattern`, `oneOf`, `anyOf`, `not`, `$ref`
//! (local `#/$defs/...`), `$defs`; annotations are ignored.

use crate::tree::{Diag, Key, Node, Pos, Value};
use regex_lite::Regex;
use std::collections::HashMap;
use zdef::enums::{self, Category, Feature, Platform, StateClass, TuyaType};
use zdef::keys::PART_NAMES;

// ------------------------------------------------------------------ builders

fn o(entries: Vec<(&str, Node)>) -> Node {
    Node::synth(Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Key::Str(k.into()), Pos::default(), v))
            .collect(),
    ))
}
fn s(x: &str) -> Node {
    Node::str(x)
}
fn i(x: i64) -> Node {
    Node::int(x)
}
fn arr(v: Vec<Node>) -> Node {
    Node::synth(Value::Seq(v))
}
fn r(name: &str) -> Node {
    o(vec![("$ref", s(&format!("#/$defs/{name}")))])
}
fn ty(t: &str) -> Node {
    o(vec![("type", s(t))])
}
fn int_range(min: i64, max: i64) -> Node {
    o(vec![
        ("type", s("integer")),
        ("minimum", i(min)),
        ("maximum", i(max)),
    ])
}
fn enum_of(v: &[&str]) -> Node {
    o(vec![("enum", arr(v.iter().map(|x| s(x)).collect()))])
}
/// Enum of a closed `zdef` string set (encoded as its integer code).
macro_rules! enum_all {
    ($t:ty) => {
        enum_of(&<$t>::ALL.iter().map(|x| x.as_str()).collect::<Vec<_>>())
    };
}
fn list_of(item: Node) -> Node {
    o(vec![
        ("type", s("array")),
        ("minItems", i(1)),
        ("items", item),
    ])
}
fn obj(props: Vec<(&str, Node)>, required: &[&str]) -> Node {
    let mut e = vec![
        ("type", s("object")),
        ("additionalProperties", Node::synth(Value::Bool(false))),
        ("properties", o(props)),
    ];
    if !required.is_empty() {
        e.push(("required", arr(required.iter().map(|x| s(x)).collect())));
    }
    o(e)
}
fn one_of(v: Vec<Node>) -> Node {
    o(vec![("oneOf", arr(v))])
}
fn int_keyed(value: Node) -> Node {
    o(vec![
        ("type", s("object")),
        ("minProperties", i(1)),
        ("propertyNames", o(vec![("pattern", s(INT_KEY))])),
        ("additionalProperties", value),
    ])
}

const INT_KEY: &str = "^(0|-?[1-9][0-9]*|0x[0-9a-fA-F]+)$";
/// `map` keys: integers or booleans (`true` = 1, `false` = 0).
const MAP_KEY: &str = "^(0|-?[1-9][0-9]*|0x[0-9a-fA-F]+|true|false)$";

fn entity_props(full: bool) -> Vec<(&'static str, Node)> {
    let platforms: Vec<&str> = Platform::ALL.iter().map(|p| p.as_str()).collect();
    let mut v = Vec::new();
    if full {
        v.push((
            "key",
            o(vec![
                ("type", s("string")),
                ("pattern", s("^[a-z][a-z0-9_]*$")),
                ("maxLength", i(64)),
            ]),
        ));
        v.push(("platform", enum_of(&platforms)));
    }
    v.extend([
        ("device_class", r("device_class")),
        ("unit", r("unit")),
        (
            "state_class",
            enum_of(
                &StateClass::ALL
                    .iter()
                    .map(|x| x.as_str())
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "category",
            enum_of(&Category::ALL.iter().map(|x| x.as_str()).collect::<Vec<_>>()),
        ),
        ("name", ty("string")),
        ("ep", int_range(1, 254)),
        (
            "zcl",
            obj(
                vec![
                    ("cluster", r("u16")),
                    ("attr", r("u16")),
                    ("type", r("zcl_type")),
                    ("mfr", r("u16")),
                    ("ep", int_range(1, 254)),
                ],
                &["cluster", "attr", "type"],
            ),
        ),
        ("tuya", r("tuya_dp")),
        (
            "struct",
            obj(
                vec![
                    ("cluster", r("u16")),
                    ("attr", r("u16")),
                    ("mfr", r("u16")),
                    ("ep", int_range(1, 254)),
                    (
                        "codec",
                        enum_of(
                            &zdef::model::STRUCT_CODECS
                                .iter()
                                .map(|c| c.as_str())
                                .collect::<Vec<_>>(),
                        ),
                    ),
                    ("tag", ty("integer")),
                ],
                &["cluster", "attr", "codec"],
            ),
        ),
        (
            "gp",
            one_of(vec![
                obj(
                    vec![(
                        "report",
                        obj(
                            vec![
                                ("cluster", r("u16")),
                                ("attr", r("u16")),
                                ("type", r("zcl_type")),
                                ("mfr", r("u16")),
                            ],
                            &["cluster", "attr", "type"],
                        ),
                    )],
                    &["report"],
                ),
                obj(vec![("cmd", r("u8"))], &["cmd"]),
            ]),
        ),
        ("from", r("expr")),
        ("to", r("expr")),
        ("mul", ty("number")),
        (
            "div",
            o(vec![
                ("type", s("number")),
                ("not", o(vec![("const", i(0))])),
            ]),
        ),
        ("offset", ty("number")),
        ("precision", int_range(0, 15)),
        ("invalid", ty("number")),
        (
            "map",
            o(vec![
                ("type", s("object")),
                ("minProperties", i(1)),
                ("propertyNames", o(vec![("pattern", s(MAP_KEY))])),
                ("additionalProperties", ty("string")),
            ]),
        ),
        ("value_on", r("scalar")),
        ("value_off", r("scalar")),
        ("min", ty("number")),
        ("max", ty("number")),
        ("step", ty("number")),
        ("mode", enum_all!(zdef::enums::Mode)),
        (
            "access",
            o(vec![("type", s("string")), ("pattern", s("^[rwp]{1,3}$"))]),
        ),
        ("report", r("report")),
        ("enabled", ty("boolean")),
        ("reset_after", r("expr")),
        (
            "parts",
            o(vec![
                ("type", s("object")),
                ("minProperties", i(1)),
                ("propertyNames", enum_of(PART_NAMES)),
                ("additionalProperties", r("part")),
            ]),
        ),
        ("event_types", list_of(ty("string"))),
        ("triggers", list_of(r("trigger"))),
        ("press", r("press")),
        ("codec", r("codec")),
        ("present_if", r("expr")),
        ("attrs", r("named_exprs")),
        ("hub", obj(vec![("default", r("scalar"))], &[])),
        ("invert", ty("boolean")),
    ]);
    v
}

/// The JSON Schema of the v1 source format.
pub fn schema() -> Node {
    let feature_names: Vec<&str> = Feature::ALL.iter().map(|f| f.as_str()).collect();
    let zcl_types: Vec<&str> = enums::ZCL_TYPES.iter().map(|t| t.0).collect();
    let tuya_types: Vec<&str> = TuyaType::ALL.iter().map(|t| t.as_str()).collect();
    let name_match = one_of(vec![
        ty("string"),
        obj(vec![("any", list_of(ty("string")))], &["any"]),
        obj(
            vec![(
                "prefix",
                o(vec![("type", s("string")), ("minLength", i(1))]),
            )],
            &["prefix"],
        ),
    ]);
    let range = o(vec![
        ("type", s("object")),
        ("additionalProperties", Node::synth(Value::Bool(false))),
        ("minProperties", i(1)),
        (
            "properties",
            o(vec![("min", ty("integer")), ("max", ty("integer"))]),
        ),
    ]);
    let cmd_ref = obj(
        vec![
            ("cluster", r("u16")),
            ("id", r("u8")),
            ("mfr", r("u16")),
            ("ep", int_range(1, 254)),
        ],
        &["cluster", "id"],
    );
    let write = obj(
        vec![
            ("cluster", r("u16")),
            ("attr", r("u16")),
            ("type", r("zcl_type")),
            ("value", r("scalar")),
            ("mfr", r("u16")),
            ("ep", int_range(1, 254)),
        ],
        &["cluster", "attr", "type", "value"],
    );
    let clause = obj(
        vec![
            ("manufacturer", r("name_match")),
            ("model", r("name_match")),
            ("mfr_code", r("u16")),
            ("app_version", r("range")),
            ("stack_version", r("range")),
            ("hw_version", r("range")),
            (
                "sw_build",
                one_of(vec![
                    ty("string"),
                    obj(vec![("prefix", ty("string"))], &["prefix"]),
                ]),
            ),
            ("power", enum_of(&["mains", "battery"])),
            ("logical_type", enum_all!(zdef::enums::LogicalType)),
            ("x-priority", ty("integer")),
            (
                "endpoints",
                int_keyed(obj(
                    vec![
                        ("profile", r("u16")),
                        ("device_id", r("u16")),
                        ("in", list_of(r("u16"))),
                        ("out", list_of(r("u16"))),
                    ],
                    &[],
                )),
            ),
            (
                "gp",
                obj(
                    vec![
                        ("device_id", r("u8")),
                        ("manufacturer_id", r("u16")),
                        (
                            "model_id",
                            one_of(vec![
                                r("u16"),
                                obj(
                                    vec![("value", r("u16")), ("mask", r("u16"))],
                                    &["value", "mask"],
                                ),
                            ]),
                        ),
                        (
                            "src_id",
                            o(vec![
                                ("type", s("array")),
                                ("prefixItems", arr(vec![r("u32"), r("u32")])),
                                ("minItems", i(2)),
                                ("maxItems", i(2)),
                            ]),
                        ),
                        (
                            "ieee",
                            o(vec![
                                ("type", s("string")),
                                ("pattern", s("^[0-9a-fA-F:]{1,23}$")),
                            ]),
                        ),
                        ("commands", list_of(r("u8"))),
                        (
                            "clusters",
                            obj(
                                vec![("server", list_of(r("u16"))), ("client", list_of(r("u16")))],
                                &[],
                            ),
                        ),
                    ],
                    &[],
                ),
            ),
        ],
        &[],
    );
    let configure = one_of(vec![
        obj(
            vec![(
                "bind",
                obj(
                    vec![("ep", int_range(1, 254)), ("clusters", list_of(r("u16")))],
                    &["clusters"],
                ),
            )],
            &["bind"],
        ),
        obj(vec![("write", write.clone())], &["write"]),
        obj(
            vec![(
                "read",
                obj(
                    vec![
                        ("ep", int_range(1, 254)),
                        ("cluster", r("u16")),
                        ("attrs", list_of(r("u16"))),
                        ("mfr", r("u16")),
                    ],
                    &["cluster", "attrs"],
                ),
            )],
            &["read"],
        ),
        obj(
            vec![(
                "report",
                obj(
                    vec![
                        ("ep", int_range(1, 254)),
                        ("cluster", r("u16")),
                        ("attr", r("u16")),
                        ("type", r("zcl_type")),
                        ("report", r("report")),
                        ("mfr", r("u16")),
                    ],
                    &["cluster", "attr", "type", "report"],
                ),
            )],
            &["report"],
        ),
        obj(
            vec![(
                "cmd",
                obj(
                    vec![
                        ("cluster", r("u16")),
                        ("id", r("u8")),
                        ("mfr", r("u16")),
                        ("ep", int_range(1, 254)),
                        ("payload", r("expr")),
                    ],
                    &["cluster", "id"],
                ),
            )],
            &["cmd"],
        ),
        obj(
            vec![(
                "group",
                obj(
                    vec![("ep", int_range(1, 254)), ("group", r("u16"))],
                    &["group"],
                ),
            )],
            &["group"],
        ),
        obj(
            vec![(
                "checkin",
                obj(vec![("interval_qs", r("u32"))], &["interval_qs"]),
            )],
            &["checkin"],
        ),
    ]);
    let press = one_of(vec![
        obj(
            vec![("cmd", cmd_ref.clone()), ("payload", r("expr"))],
            &["cmd"],
        ),
        obj(vec![("write", write)], &["write"]),
        obj(
            vec![("tuya", r("tuya_dp")), ("value", r("scalar"))],
            &["tuya", "value"],
        ),
    ]);
    let trigger = o(vec![
        ("type", s("object")),
        ("additionalProperties", Node::synth(Value::Bool(false))),
        (
            "properties",
            o(vec![
                ("cmd", cmd_ref),
                ("gp_cmd", r("u8")),
                ("tuya", r("tuya_dp")),
                ("when", r("expr")),
                ("event", r("expr")),
                ("attrs", r("named_exprs")),
            ]),
        ),
        ("required", arr(vec![s("event")])),
        (
            "oneOf",
            arr(vec![
                o(vec![("required", arr(vec![s("cmd")]))]),
                o(vec![("required", arr(vec![s("gp_cmd")]))]),
                o(vec![("required", arr(vec![s("tuya")]))]),
            ]),
        ),
    ]);
    let codec = one_of(vec![
        enum_of(&["zcl_tagged", "bcd_time", "hhmm", "ascii", "utf16be_hex"]),
        obj(
            vec![(
                "tuya_schedule_day",
                obj(
                    vec![
                        ("day", int_range(1, 7)),
                        ("transitions", int_range(1, 10)),
                        ("variant", enum_all!(zdef::enums::ScheduleVariant)),
                    ],
                    &["day", "transitions"],
                ),
            )],
            &["tuya_schedule_day"],
        ),
        obj(
            vec![(
                "tuya_schedule_week",
                obj(
                    vec![
                        ("variant", enum_all!(zdef::enums::ScheduleVariant)),
                        ("transitions", int_range(1, 10)),
                        ("scale", int_range(1, 1000)),
                        ("min", int_range(-1000, 1000)),
                        ("max", int_range(-1000, 1000)),
                    ],
                    &["variant"],
                ),
            )],
            &["tuya_schedule_week"],
        ),
        obj(
            vec![("bitmap_flags", int_keyed(ty("string")))],
            &["bitmap_flags"],
        ),
    ]);
    let entity = obj(entity_props(true), &["key", "platform"]);
    let part = obj(entity_props(false), &[]);
    let device = obj(
        vec![
            ("power", enum_all!(zdef::enums::Power)),
            ("sleepy", ty("boolean")),
            (
                "battery",
                one_of(vec![
                    enum_all!(zdef::enums::BatteryCurve),
                    obj(
                        vec![(
                            "voltage_to_percent",
                            o(vec![
                                ("type", s("array")),
                                ("prefixItems", arr(vec![r("u32"), r("u32")])),
                                ("minItems", i(2)),
                                ("maxItems", i(2)),
                            ]),
                        )],
                        &["voltage_to_percent"],
                    ),
                ]),
            ),
            ("checkin_s", r("u32")),
            ("availability", obj(vec![("timeout_s", r("u32"))], &[])),
            ("ota", ty("boolean")),
        ],
        &[],
    );
    let generic = obj(
        vec![
            ("exclude", list_of(ty("string"))),
            (
                "patch",
                o(vec![
                    ("type", s("object")),
                    ("minProperties", i(1)),
                    ("additionalProperties", r("part")),
                ]),
            ),
            ("disable", ty("boolean")),
            ("only_endpoints", list_of(int_range(1, 254))),
        ],
        &[],
    );
    let tuya = obj(
        vec![
            (
                "time_sync",
                one_of(vec![
                    enum_all!(zdef::enums::TimeSync),
                    o(vec![("const", i(1970))]),
                    o(vec![("const", i(2000))]),
                ]),
            ),
            ("force_time_sync_s", r("u32")),
            ("magic_packet", ty("boolean")),
            (
                "query",
                one_of(vec![
                    list_of(one_of(vec![
                        enum_of(&["configure", "announce"]),
                        obj(vec![("interval_s", r("u32"))], &["interval_s"]),
                    ])),
                    obj(vec![("interval_s", r("u32"))], &["interval_s"]),
                ]),
            ),
            ("bind_basic", ty("boolean")),
            ("gateway_status", ty("boolean")),
            ("send", enum_all!(zdef::enums::TuyaSend)),
            ("mcu_version", ty("boolean")),
            ("optimistic", ty("boolean")),
            ("dedup", ty("boolean")),
        ],
        &[],
    );
    let top_props = vec![
        ("zdef", o(vec![("const", i(1))])),
        ("zdef_fragment", o(vec![("const", i(1))])),
        (
            "id",
            o(vec![
                ("type", s("string")),
                ("pattern", s("^[a-z0-9._/-]{3,64}$")),
            ]),
        ),
        ("revision", r("u32")),
        ("vendor", ty("string")),
        ("model", ty("string")),
        ("description", ty("string")),
        ("source", ty("string")),
        ("mfr", r("u16")),
        ("match", list_of(clause)),
        (
            "white_label",
            list_of(obj(
                vec![
                    ("vendor", ty("string")),
                    ("model", ty("string")),
                    ("manufacturer", list_of(ty("string"))),
                    ("model_id", list_of(ty("string"))),
                ],
                &[],
            )),
        ),
        ("requires", list_of(enum_of(&feature_names))),
        ("priority", int_range(-1000, 1000)),
        ("self_description", enum_all!(zdef::enums::SelfDescription)),
        ("device", device),
        ("generic", generic),
        ("endpoints", int_keyed(ty("string"))),
        ("tuya", tuya),
        ("configure", list_of(configure)),
        ("entities", list_of(r("entity"))),
        (
            "gp",
            obj(
                vec![
                    ("security_min", int_range(0, 3)),
                    ("report_interval_s", r("u32")),
                    ("ignore_commands", list_of(r("u8"))),
                ],
                &[],
            ),
        ),
        (
            "ignore",
            list_of(obj(
                vec![
                    ("ep", int_range(1, 254)),
                    ("cluster", r("u16")),
                    ("cmd", r("u8")),
                ],
                &["cluster"],
            )),
        ),
        (
            "templates",
            list_of(obj(
                vec![
                    ("name", enum_all!(zdef::enums::TemplateName)),
                    ("params", ty("object")),
                ],
                &["name"],
            )),
        ),
        ("x-wasm", ty("object")),
        ("x-partial", ty("boolean")),
    ];
    let defs = o(vec![
        ("u8", int_range(0, 0xff)),
        ("u16", int_range(0, 0xffff)),
        ("u32", int_range(0, 0xffff_ffff)),
        (
            "expr",
            o(vec![(
                "type",
                arr(vec![s("string"), s("number"), s("boolean")]),
            )]),
        ),
        (
            "scalar",
            o(vec![(
                "type",
                arr(vec![s("null"), s("boolean"), s("number"), s("string")]),
            )]),
        ),
        (
            "named_exprs",
            o(vec![
                ("type", s("object")),
                ("minProperties", i(1)),
                ("additionalProperties", r("expr")),
            ]),
        ),
        ("zcl_type", enum_of(&zcl_types)),
        (
            "tuya_dp",
            o(vec![
                ("type", s("array")),
                ("prefixItems", arr(vec![r("u8"), enum_of(&tuya_types)])),
                ("minItems", i(2)),
                ("maxItems", i(2)),
            ]),
        ),
        (
            "report",
            one_of(vec![
                o(vec![("const", s("off"))]),
                o(vec![
                    ("type", s("array")),
                    ("prefixItems", arr(vec![r("u16"), r("u16"), ty("number")])),
                    ("minItems", i(2)),
                    ("maxItems", i(3)),
                ]),
            ]),
        ),
        ("device_class", enum_of(enums::DEVICE_CLASSES)),
        ("unit", enum_of(enums::UNITS)),
        ("name_match", name_match),
        ("range", range),
        ("trigger", trigger),
        ("press", press),
        ("codec", codec),
        ("entity", entity),
        ("part", part),
    ]);
    let definition = o(vec![
        (
            "required",
            arr(["zdef", "id", "revision", "vendor", "model", "match"]
                .iter()
                .map(|x| s(x))
                .collect()),
        ),
        ("not", o(vec![("required", arr(vec![s("zdef_fragment")]))])),
    ]);
    let fragment = o(vec![
        ("required", arr(vec![s("zdef_fragment")])),
        (
            "not",
            o(vec![(
                "anyOf",
                arr(["zdef", "id", "match", "white_label", "tuya", "gp"]
                    .iter()
                    .map(|x| o(vec![("required", arr(vec![s(x)]))]))
                    .collect()),
            )]),
        ),
    ]);
    o(vec![
        ("$schema", s("https://json-schema.org/draft/2020-12/schema")),
        (
            "$id",
            s("https://github.com/faronov/zigbee-rs-hub/schema/zdef-v1.schema.json"),
        ),
        ("title", s("zdef v1 device definition (source form)")),
        (
            "description",
            s(
                "Generated by `zdefc schema` from the zdef tables; see docs/device-support.md sections 6-8, 12, 13.",
            ),
        ),
        ("type", s("object")),
        ("additionalProperties", Node::synth(Value::Bool(false))),
        ("properties", o(top_props)),
        ("oneOf", arr(vec![definition, fragment])),
        ("$defs", defs),
    ])
}

/// Published schema text.
pub fn schema_json() -> String {
    crate::emit::json(&schema())
}

// ------------------------------------------------------------------ validator

pub struct Validator {
    root: Node,
    regex: HashMap<String, Regex>,
}

fn json_type_matches(t: &str, n: &Node) -> bool {
    match (t, &n.v) {
        ("null", Value::Null)
        | ("boolean", Value::Bool(_))
        | ("string", Value::Str(_))
        | ("array", Value::Seq(_))
        | ("object", Value::Map(_)) => true,
        ("integer", Value::Int(_)) => true,
        ("integer", Value::Float(f)) => f.fract() == 0.0,
        ("number", Value::Int(_) | Value::Float(_)) => true,
        _ => false,
    }
}

fn num_of(n: &Node) -> Option<f64> {
    match n.v {
        Value::Int(i) => Some(i as f64),
        Value::Float(f) => Some(f),
        _ => None,
    }
}

fn json_eq(a: &Node, b: &Node) -> bool {
    match (&a.v, &b.v) {
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Null, Value::Null) => true,
        _ => match (num_of(a), num_of(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

impl Validator {
    pub fn new(schema: Node) -> Self {
        let mut v = Validator {
            root: schema,
            regex: HashMap::new(),
        };
        let mut pats = Vec::new();
        collect_patterns(&v.root, &mut pats);
        for p in pats {
            let re = Regex::new(&p).expect("schema pattern");
            v.regex.insert(p, re);
        }
        v
    }

    fn resolve<'a>(&'a self, s: &'a Node) -> &'a Node {
        if let Some(Value::Str(r)) = s.get("$ref").map(|n| &n.v) {
            if let Some(name) = r.strip_prefix("#/$defs/") {
                if let Some(d) = self.root.get("$defs").and_then(|d| d.get(name)) {
                    return self.resolve(d);
                }
            }
        }
        s
    }

    /// Validates `data`; returns all errors.
    pub fn validate(&self, data: &Node) -> Vec<Diag> {
        let mut errs = Vec::new();
        self.check(&self.root, data, "", &mut errs);
        errs
    }

    fn check(&self, schema: &Node, d: &Node, path: &str, errs: &mut Vec<Diag>) {
        let s = self.resolve(schema);
        let mut err = |m: String| {
            errs.push(Diag {
                pos: d.pos,
                path: if path.is_empty() {
                    "/".into()
                } else {
                    path.into()
                },
                message: m,
            })
        };
        if let Some(t) = s.get("type") {
            let ok = match &t.v {
                Value::Str(t) => json_type_matches(t, d),
                Value::Seq(ts) => ts
                    .iter()
                    .any(|t| matches!(&t.v, Value::Str(x) if json_type_matches(x, d))),
                _ => true,
            };
            if !ok {
                let want = match &t.v {
                    Value::Str(t) => t.clone(),
                    Value::Seq(ts) => ts
                        .iter()
                        .filter_map(|t| {
                            if let Value::Str(x) = &t.v {
                                Some(x.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" or "),
                    _ => String::new(),
                };
                err(format!("expected {want}, found {}", d.kind()));
                return;
            }
        }
        if let Some(Value::Seq(opts)) = s.get("enum").map(|n| &n.v) {
            if !opts.iter().any(|o| json_eq(o, d)) {
                let shown: Vec<String> = opts
                    .iter()
                    .take(12)
                    .map(|o| crate::emit::json(o).trim().to_string())
                    .collect();
                err(format!(
                    "value is not one of {}{}",
                    shown.join(", "),
                    if opts.len() > 12 { ", ..." } else { "" }
                ));
            }
        }
        if let Some(c) = s.get("const") {
            if !json_eq(c, d) {
                err(format!("value must be {}", crate::emit::json(c).trim()));
            }
        }
        if let Some(n) = num_of(d) {
            if let Some(m) = s.get("minimum").and_then(num_of) {
                if n < m {
                    err(format!("{n} is below the minimum {m}"));
                }
            }
            if let Some(m) = s.get("maximum").and_then(num_of) {
                if n > m {
                    err(format!("{n} is above the maximum {m}"));
                }
            }
        }
        if let Value::Str(st) = &d.v {
            let len = st.chars().count() as f64;
            if s.get("minLength").and_then(num_of).is_some_and(|m| len < m) {
                err("string is too short".into());
            }
            if s.get("maxLength").and_then(num_of).is_some_and(|m| len > m) {
                err("string is too long".into());
            }
            if let Some(Value::Str(p)) = s.get("pattern").map(|n| &n.v) {
                if !self.regex[p].is_match(st) {
                    err(format!("{st:?} does not match {p}"));
                }
            }
        }
        if let Value::Seq(items) = &d.v {
            if s.get("minItems")
                .and_then(num_of)
                .is_some_and(|m| (items.len() as f64) < m)
            {
                err(format!(
                    "at least {} items required",
                    s.get("minItems").and_then(num_of).unwrap_or(0.0)
                ));
            }
            if s.get("maxItems")
                .and_then(num_of)
                .is_some_and(|m| (items.len() as f64) > m)
            {
                err("too many items".into());
            }
            let prefix: &[Node] = match s.get("prefixItems").map(|n| &n.v) {
                Some(Value::Seq(p)) => p,
                _ => &[],
            };
            for (k, it) in items.iter().enumerate() {
                let p = format!("{path}/{k}");
                if let Some(ps) = prefix.get(k) {
                    self.check(ps, it, &p, errs);
                } else if let Some(is) = s.get("items") {
                    self.check(is, it, &p, errs);
                }
            }
        }
        if let Value::Map(m) = &d.v {
            let props = s.get("properties");
            if let Some(Value::Seq(req)) = s.get("required").map(|n| &n.v) {
                for rq in req {
                    if let Value::Str(rq) = &rq.v {
                        if !m.iter().any(|e| e.0.as_json() == *rq) {
                            errs.push(Diag {
                                pos: d.pos,
                                path: if path.is_empty() {
                                    "/".into()
                                } else {
                                    path.into()
                                },
                                message: format!("missing required field {rq:?}"),
                            });
                        }
                    }
                }
            }
            if s.get("minProperties")
                .and_then(num_of)
                .is_some_and(|x| (m.len() as f64) < x)
            {
                errs.push(Diag {
                    pos: d.pos,
                    path: path.into(),
                    message: "object is empty".into(),
                });
            }
            for (k, kp, v) in m {
                let key = k.as_json();
                let p = format!("{path}/{key}");
                if let Some(pn) = s.get("propertyNames") {
                    let kn = Node::new(Value::Str(key.clone()), *kp);
                    self.check(pn, &kn, &p, errs);
                }
                if let Some(ps) = props.and_then(|pr| pr.get(&key)) {
                    self.check(ps, v, &p, errs);
                } else if let Some(ap) = s.get("additionalProperties") {
                    match ap.v {
                        Value::Bool(false) => errs.push(Diag {
                            pos: *kp,
                            path: p,
                            message: format!("unknown field {key:?}"),
                        }),
                        Value::Bool(true) => {}
                        _ => self.check(ap, v, &p, errs),
                    }
                }
            }
        }
        if let Some(Value::Seq(alts)) = s.get("oneOf").map(|n| &n.v) {
            let results: Vec<Vec<Diag>> = alts
                .iter()
                .map(|a| {
                    let mut e = Vec::new();
                    self.check(a, d, path, &mut e);
                    e
                })
                .collect();
            let ok = results.iter().filter(|r| r.is_empty()).count();
            if ok == 0 {
                let best = results
                    .into_iter()
                    .min_by_key(|r| r.len())
                    .unwrap_or_default();
                errs.extend(best);
            } else if ok > 1 {
                errs.push(Diag {
                    pos: d.pos,
                    path: path.into(),
                    message: "value matches more than one allowed form".into(),
                });
            }
        }
        if let Some(Value::Seq(alts)) = s.get("anyOf").map(|n| &n.v) {
            let mut best: Option<Vec<Diag>> = None;
            for a in alts {
                let mut e = Vec::new();
                self.check(a, d, path, &mut e);
                if e.is_empty() {
                    best = None;
                    break;
                }
                if best.as_ref().is_none_or(|b| e.len() < b.len()) {
                    best = Some(e);
                }
            }
            if let Some(b) = best {
                errs.extend(b);
            }
        }
        if let Some(n) = s.get("not") {
            let mut e = Vec::new();
            self.check(n, d, path, &mut e);
            if e.is_empty() {
                errs.push(Diag {
                    pos: d.pos,
                    path: path.into(),
                    message: "value has a form that is not allowed here".into(),
                });
            }
        }
    }
}

fn collect_patterns(n: &Node, out: &mut Vec<String>) {
    match &n.v {
        Value::Map(m) => {
            for (k, _, v) in m {
                if k.as_str() == Some("pattern") {
                    if let Value::Str(p) = &v.v {
                        out.push(p.clone());
                    }
                }
                collect_patterns(v, out);
            }
        }
        Value::Seq(v) => v.iter().for_each(|x| collect_patterns(x, out)),
        _ => {}
    }
}

/// The validator for the built-in schema.
pub fn validator() -> &'static Validator {
    static V: std::sync::OnceLock<Validator> = std::sync::OnceLock::new();
    V.get_or_init(|| Validator::new(schema()))
}

//! The daemon's OpenAPI document restated as OpenAPI 3.0.3.
//!
//! [`super::document`] is OpenAPI 3.1, which is what utoipa writes and what
//! viewers such as Scalar read. Tools that only understand 3.0 semantics
//! (Cloudflare API Shield's schema validation among them) misread the 3.1
//! forms the document uses, so the daemon also serves this translation at
//! [`OPENAPI_V30_PATH`]. The translation is sans-IO: [`downgrade`] maps one
//! JSON document to another and fails, naming the place, on any 3.1 form it
//! has no 3.0 equivalent for, so a new construct cannot slip through
//! silently. It works on [`Json`], which keeps object keys in document
//! order, so the 3.0 copy lists paths and fields as the 3.1 one does.
//!
//! What changes:
//!
//! - `type: [T, "null"]` becomes `type: T` with `nullable: true`.
//! - A `oneOf` / `anyOf` with a `{"type": "null"}` branch drops that branch
//!   and sets `nullable: true`. A single remaining inline branch is folded
//!   in; a single remaining `$ref` to a component schema is replaced by a
//!   copy of that schema, because 3.0 ignores `nullable` beside a bare
//!   `allOf` and would refuse `null`.
//! - A `$ref` with sibling keys (3.0 ignores siblings) moves into a
//!   one-item `allOf` so the siblings, such as `description`, still apply.
//! - A nullable `enum` gets `null` among its values, as 3.0.3 requires.
//! - `const` becomes a one-value `enum`, `examples` becomes `example` (the
//!   first one), and numeric `exclusiveMinimum` / `exclusiveMaximum` become
//!   `minimum` / `maximum` with the boolean flag.
//! - `propertyNames: {"type": "string"}` says nothing (JSON keys are always
//!   strings) and is dropped; any other `propertyNames` is refused.

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Path the 3.0.3 document is served at.
pub const OPENAPI_V30_PATH: &str = "/api/openapi-3.0.json";

/// The OpenAPI version the translation writes.
const VERSION: &str = "3.0.3";

/// Prefix of a local component schema reference.
const SCHEMA_REF_PREFIX: &str = "#/components/schemas/";

/// Keywords an OpenAPI 3.0.3 Schema Object may carry, besides `x-*`
/// extensions.
const SCHEMA_KEYWORDS_V30: &[&str] = &[
    "title",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxProperties",
    "minProperties",
    "required",
    "enum",
    "type",
    "allOf",
    "oneOf",
    "anyOf",
    "not",
    "items",
    "properties",
    "additionalProperties",
    "description",
    "format",
    "default",
    "nullable",
    "discriminator",
    "readOnly",
    "writeOnly",
    "xml",
    "externalDocs",
    "example",
    "deprecated",
];

/// The non-null JSON types a 3.0 `type` may name.
const TYPES_V30: &[&str] = &["string", "number", "integer", "boolean", "array", "object"];

/// A JSON value whose objects keep their keys in document order.
///
/// `serde_json::Value` sorts object keys in this workspace (its
/// `preserve_order` feature is off, and turning it on would change every
/// crate's JSON), so the translation reads and writes this instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any JSON number, kept exactly as parsed.
    Number(serde_json::Number),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, keys in document order.
    Object(IndexMap<String, Json>),
}

/// An ordered JSON object.
type Object = IndexMap<String, Json>;

impl Json {
    /// The string, when this is one.
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// The object, when this is one.
    fn as_object(&self) -> Option<&Object> {
        match self {
            Json::Object(map) => Some(map),
            _ => None,
        }
    }

    /// The value at `key`, when this is an object holding it.
    fn get(&self, key: &str) -> Option<&Json> {
        self.as_object().and_then(|map| map.get(key))
    }
}

/// Why a 3.1 document could not be restated as 3.0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DowngradeError {
    /// A keyword or field with no 3.0 equivalent.
    Unsupported {
        /// JSON Pointer to the object that holds it.
        pointer: String,
        /// The keyword or field name.
        keyword: String,
    },
    /// A value whose shape the translation does not accept, such as a
    /// `type` list naming two non-null types or a schema that is not an
    /// object.
    Shape {
        /// JSON Pointer to the value.
        pointer: String,
        /// What was expected there.
        expected: &'static str,
    },
}

impl fmt::Display for DowngradeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DowngradeError::Unsupported { pointer, keyword } => write!(
                f,
                "`{keyword}` at `{pointer}` has no OpenAPI 3.0 equivalent"
            ),
            DowngradeError::Shape { pointer, expected } => {
                write!(f, "expected {expected} at `{pointer}`")
            }
        }
    }
}

impl std::error::Error for DowngradeError {}

/// What every step of the translation can see.
struct Context<'a> {
    /// The 3.1 document's `components.schemas`, for inlining a nullable
    /// reference.
    schemas: Option<&'a Object>,
    /// Component names being inlined, outermost first, so a schema that
    /// refers to itself is not inlined forever.
    inlining: Vec<String>,
}

/// Restates an OpenAPI 3.1 document as OpenAPI 3.0.3.
///
/// # Arguments
///
/// * `doc` - The 3.1 document.
///
/// # Returns
///
/// The 3.0.3 document: the same paths, operations, components and texts in
/// the same order, with every schema in 3.0 form.
///
/// # Errors
///
/// [`DowngradeError`] when the document uses a 3.1 form without a 3.0
/// equivalent (`webhooks`, `prefixItems`, a `type` list of two non-null
/// types, …) or a schema is not an object.
pub fn downgrade(doc: &Json) -> Result<Json, DowngradeError> {
    let Json::Object(root) = doc else {
        return Err(shape("", "a JSON object"));
    };
    for keyword in ["webhooks", "jsonSchemaDialect"] {
        if root.contains_key(keyword) {
            return Err(unsupported("", keyword));
        }
    }
    if doc.get("info").and_then(|i| i.get("summary")).is_some() {
        return Err(unsupported("/info", "summary"));
    }
    if doc
        .get("info")
        .and_then(|i| i.get("license"))
        .and_then(|l| l.get("identifier"))
        .is_some()
    {
        return Err(unsupported("/info/license", "identifier"));
    }
    if doc
        .get("components")
        .and_then(|c| c.get("pathItems"))
        .is_some()
    {
        return Err(unsupported("/components", "pathItems"));
    }
    let mut cx = Context {
        schemas: doc
            .get("components")
            .and_then(|c| c.get("schemas"))
            .and_then(Json::as_object),
        inlining: Vec::new(),
    };
    let mut out = Object::new();
    for (key, value) in root {
        let pointer = format!("/{}", escape(key));
        let value = match key.as_str() {
            "openapi" => Json::String(VERSION.to_string()),
            "components" => components(&mut cx, value, &pointer)?,
            _ => non_schema(&mut cx, value, &pointer)?,
        };
        out.insert(key.clone(), value);
    }
    Ok(Json::Object(out))
}

/// Translates the `components` object: its `schemas` map holds schemas, and
/// everything else is walked for nested `schema` fields.
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `value` - The `components` object.
/// * `pointer` - Its JSON Pointer.
///
/// # Returns
///
/// The translated object.
///
/// # Errors
///
/// As [`downgrade`].
fn components(cx: &mut Context<'_>, value: &Json, pointer: &str) -> Result<Json, DowngradeError> {
    let Json::Object(map) = value else {
        return Err(shape(pointer, "a JSON object"));
    };
    let mut out = Object::new();
    for (key, inner) in map {
        let at = format!("{pointer}/{}", escape(key));
        let inner = if key == "schemas" {
            schema_map(cx, inner, &at)?
        } else {
            non_schema(cx, inner, &at)?
        };
        out.insert(key.clone(), inner);
    }
    Ok(Json::Object(out))
}

/// Walks a part of the document that is not a schema, translating every
/// value held under a `schema` key.
///
/// `example` and `examples` values are data, not document structure, so
/// they are copied unchanged.
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `value` - The value to walk.
/// * `pointer` - Its JSON Pointer.
///
/// # Returns
///
/// The translated value.
///
/// # Errors
///
/// As [`downgrade`].
fn non_schema(cx: &mut Context<'_>, value: &Json, pointer: &str) -> Result<Json, DowngradeError> {
    match value {
        Json::Object(map) => {
            let mut out = Object::new();
            for (key, inner) in map {
                let at = format!("{pointer}/{}", escape(key));
                let inner = match key.as_str() {
                    "schema" => schema(cx, inner, &at)?,
                    "example" | "examples" => inner.clone(),
                    _ => non_schema(cx, inner, &at)?,
                };
                out.insert(key.clone(), inner);
            }
            Ok(Json::Object(out))
        }
        Json::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| non_schema(cx, item, &format!("{pointer}/{i}")))
            .collect::<Result<Vec<_>, _>>()
            .map(Json::Array),
        other => Ok(other.clone()),
    }
}

/// Translates a map whose values are all schemas (`properties`,
/// `components.schemas`).
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `value` - The map.
/// * `pointer` - Its JSON Pointer.
///
/// # Returns
///
/// The map with each schema translated.
///
/// # Errors
///
/// As [`downgrade`].
fn schema_map(cx: &mut Context<'_>, value: &Json, pointer: &str) -> Result<Json, DowngradeError> {
    let Json::Object(map) = value else {
        return Err(shape(pointer, "a map of schemas"));
    };
    let mut out = Object::new();
    for (key, inner) in map {
        out.insert(
            key.clone(),
            schema(cx, inner, &format!("{pointer}/{}", escape(key)))?,
        );
    }
    Ok(Json::Object(out))
}

/// Translates a list of schemas (`allOf`, `oneOf`, `anyOf`).
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `value` - The list.
/// * `pointer` - Its JSON Pointer.
///
/// # Returns
///
/// The list with each schema translated.
///
/// # Errors
///
/// As [`downgrade`].
fn schema_list(cx: &mut Context<'_>, value: &Json, pointer: &str) -> Result<Json, DowngradeError> {
    let Json::Array(items) = value else {
        return Err(shape(pointer, "a list of schemas"));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| schema(cx, item, &format!("{pointer}/{i}")))
        .collect::<Result<Vec<_>, _>>()
        .map(Json::Array)
}

/// Whether a schema is exactly `{"type": "null"}`.
fn is_null_schema(value: &Json) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.get("type").and_then(Json::as_str) == Some("null"))
}

/// Adds `branch`'s keys to `map`; a key both hold keeps `map`'s value.
fn merge_under(map: &mut Object, branch: &Object) {
    for (key, inner) in branch {
        if !map.contains_key(key) {
            map.insert(key.clone(), inner.clone());
        }
    }
}

/// The component a `$ref` names, when it is a local schema that can be
/// copied in place of the reference.
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `branch` - The `{"$ref": …}` schema.
///
/// # Returns
///
/// The component's name and 3.1 schema, or `None` for another kind of
/// reference, an unknown name, a component that is itself only a
/// reference, or one already being inlined.
fn inlinable<'a>(cx: &Context<'a>, branch: &Object) -> Option<(String, &'a Object)> {
    let name = branch
        .get("$ref")?
        .as_str()?
        .strip_prefix(SCHEMA_REF_PREFIX)?;
    let target = cx.schemas?.get(name)?.as_object()?;
    if target.contains_key("$ref") || cx.inlining.iter().any(|n| n == name) {
        return None;
    }
    Some((name.to_string(), target))
}

/// Translates one schema and everything nested in it.
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `value` - The 3.1 schema.
/// * `pointer` - Its JSON Pointer.
///
/// # Returns
///
/// The 3.0 schema.
///
/// # Errors
///
/// As [`downgrade`].
fn schema(cx: &mut Context<'_>, value: &Json, pointer: &str) -> Result<Json, DowngradeError> {
    let Json::Object(input) = value else {
        return Err(shape(pointer, "a schema object"));
    };
    let mut map = input.clone();
    let mut nullable = false;
    // A component copied in for a nullable reference; popped once its
    // nested schemas are translated.
    let mut inlined = None;

    // Null branches first: `{"type": "null"}` has no 3.0 form of its own.
    for keyword in ["oneOf", "anyOf"] {
        let Some(Json::Array(branches)) = map.get(keyword) else {
            continue;
        };
        if !branches.iter().any(is_null_schema) {
            continue;
        }
        nullable = true;
        let rest: Vec<Json> = branches
            .iter()
            .filter(|b| !is_null_schema(b))
            .cloned()
            .collect();
        match rest.as_slice() {
            [] => return Err(shape(pointer, "a non-null branch")),
            [Json::Object(branch)] => {
                let index = map.get_index_of(keyword).unwrap_or(map.len());
                map.shift_remove(keyword);
                if !branch.contains_key("$ref") {
                    for (key, inner) in branch {
                        if map.contains_key(key) {
                            return Err(unsupported(pointer, key));
                        }
                        map.insert(key.clone(), inner.clone());
                    }
                } else if let Some((name, target)) = inlinable(cx, branch) {
                    // The component's keys, then the branch's siblings
                    // (`description`), with this schema's own keys winning.
                    let mut copy = branch.clone();
                    copy.shift_remove("$ref");
                    merge_under(&mut map, &copy);
                    merge_under(&mut map, target);
                    cx.inlining.push(name);
                    inlined = Some(());
                } else {
                    if map.contains_key("allOf") {
                        return Err(unsupported(pointer, "allOf"));
                    }
                    let mut only = Object::new();
                    for (key, inner) in branch {
                        if key == "$ref" {
                            only.insert(key.clone(), inner.clone());
                        } else if !map.contains_key(key) {
                            map.insert(key.clone(), inner.clone());
                        }
                    }
                    let index = index.min(map.len());
                    map.shift_insert(
                        index,
                        "allOf".to_string(),
                        Json::Array(vec![Json::Object(only)]),
                    );
                }
            }
            [_] => return Err(shape(pointer, "a schema object")),
            _ => {
                map.insert(keyword.to_string(), Json::Array(rest));
            }
        }
    }

    let result = translate_keywords(cx, map, nullable, pointer);
    if inlined.is_some() {
        cx.inlining.pop();
    }
    result
}

/// The rest of [`schema`]: rewrites the keywords of one schema whose null
/// branches are already resolved, then translates its nested schemas.
///
/// # Arguments
///
/// * `cx` - The translation context.
/// * `map` - The schema's keywords.
/// * `nullable` - Whether a null branch was dropped.
/// * `pointer` - The schema's JSON Pointer.
///
/// # Returns
///
/// The 3.0 schema.
///
/// # Errors
///
/// As [`downgrade`].
fn translate_keywords(
    cx: &mut Context<'_>,
    mut map: Object,
    mut nullable: bool,
    pointer: &str,
) -> Result<Json, DowngradeError> {
    if let Some(types) = map.get("type").cloned() {
        match types {
            Json::String(t) if TYPES_V30.contains(&t.as_str()) => {}
            Json::Array(list) => {
                let mut named = Vec::new();
                for t in &list {
                    match t.as_str() {
                        Some("null") => nullable = true,
                        Some(t) if TYPES_V30.contains(&t) => named.push(t.to_string()),
                        _ => return Err(shape(&format!("{pointer}/type"), "a JSON type name")),
                    }
                }
                match named.as_slice() {
                    [only] => {
                        map.insert("type".to_string(), Json::String(only.clone()));
                    }
                    _ => {
                        return Err(shape(
                            &format!("{pointer}/type"),
                            "exactly one non-null type",
                        ))
                    }
                }
            }
            _ => return Err(shape(&format!("{pointer}/type"), "a 3.0 type name")),
        }
    }

    if let Some(index) = map.get_index_of("const") {
        if map.contains_key("enum") {
            return Err(unsupported(pointer, "const"));
        }
        if let Some((_, value)) = map.shift_remove_index(index) {
            map.shift_insert(index, "enum".to_string(), Json::Array(vec![value]));
        }
    }
    if let Some(index) = map.get_index_of("examples") {
        if let Some((_, examples)) = map.shift_remove_index(index) {
            let Json::Array(examples) = examples else {
                return Err(shape(&format!("{pointer}/examples"), "a list of examples"));
            };
            if let Some(first) = examples.into_iter().next() {
                if !map.contains_key("example") {
                    map.shift_insert(index, "example".to_string(), first);
                }
            }
        }
    }
    for (exclusive, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        if let Some(Json::Number(limit)) = map.get(exclusive).cloned() {
            if map.contains_key(bound) {
                return Err(unsupported(pointer, exclusive));
            }
            map.insert(bound.to_string(), Json::Number(limit));
            map.insert(exclusive.to_string(), Json::Bool(true));
        }
    }
    if let Some(names) = map.shift_remove("propertyNames") {
        let only_string = names.as_object().is_some_and(|n| {
            n.len() == 1 && n.get("type").and_then(Json::as_str) == Some("string")
        });
        if !only_string {
            return Err(unsupported(pointer, "propertyNames"));
        }
    }

    if nullable {
        map.insert("nullable".to_string(), Json::Bool(true));
        if let Some(Json::Array(values)) = map.get_mut("enum") {
            if !values.contains(&Json::Null) {
                values.push(Json::Null);
            }
        }
    }

    // A 3.0 Reference Object ignores siblings, so keep them beside an
    // `allOf` that holds the reference.
    if map.len() > 1 {
        if let Some(index) = map.get_index_of("$ref") {
            if map.contains_key("allOf") {
                return Err(unsupported(pointer, "$ref"));
            }
            if let Some((key, reference)) = map.shift_remove_index(index) {
                let mut only = Object::new();
                only.insert(key, reference);
                map.shift_insert(
                    index,
                    "allOf".to_string(),
                    Json::Array(vec![Json::Object(only)]),
                );
            }
        }
    }

    let mut out = Object::new();
    for (key, inner) in map {
        let at = format!("{pointer}/{}", escape(&key));
        let inner = match key.as_str() {
            "$ref" => inner,
            "properties" => schema_map(cx, &inner, &at)?,
            "items" | "not" => schema(cx, &inner, &at)?,
            "additionalProperties" => match inner {
                Json::Bool(_) => inner,
                other => schema(cx, &other, &at)?,
            },
            "allOf" | "oneOf" | "anyOf" => schema_list(cx, &inner, &at)?,
            k if k.starts_with("x-") || SCHEMA_KEYWORDS_V30.contains(&k) => inner,
            _ => return Err(unsupported(pointer, &key)),
        };
        out.insert(key, inner);
    }
    Ok(Json::Object(out))
}

/// Escapes a key for use as a JSON Pointer segment (RFC 6901).
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Builds [`DowngradeError::Unsupported`].
fn unsupported(pointer: &str, keyword: &str) -> DowngradeError {
    DowngradeError::Unsupported {
        pointer: pointer.to_string(),
        keyword: keyword.to_string(),
    }
}

/// Builds [`DowngradeError::Shape`].
fn shape(pointer: &str, expected: &'static str) -> DowngradeError {
    DowngradeError::Shape {
        pointer: pointer.to_string(),
        expected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Converts a `json!` value into a [`Json`] tree.
    fn tree(value: serde_json::Value) -> Json {
        serde_json::from_value(value).expect("valid JSON")
    }

    /// Wraps schemas in a minimal document under `components.schemas`.
    fn doc_with(schemas: serde_json::Value) -> Json {
        tree(json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {},
            "components": {"schemas": schemas}
        }))
    }

    /// Translates schema `S` through [`downgrade`].
    fn translate(schema: serde_json::Value) -> Result<Json, DowngradeError> {
        translate_beside(schema, json!({}))
    }

    /// Translates schema `S` beside other component schemas.
    fn translate_beside(
        schema: serde_json::Value,
        others: serde_json::Value,
    ) -> Result<Json, DowngradeError> {
        let mut schemas = others;
        schemas["S"] = schema;
        downgrade(&doc_with(schemas)).map(|doc| {
            doc.get("components")
                .and_then(|c| c.get("schemas"))
                .and_then(|s| s.get("S"))
                .cloned()
                .unwrap_or(Json::Null)
        })
    }

    #[test]
    fn the_version_is_3_0_3() {
        let doc = downgrade(&doc_with(json!({"S": {"type": "string"}}))).expect("downgrades");
        assert_eq!(doc.get("openapi"), Some(&Json::String("3.0.3".into())));
    }

    #[test]
    fn key_order_is_kept() {
        let text = r#"{"openapi":"3.1.0","info":{"title":"t","version":"1"},"paths":{},
            "components":{"schemas":{"Z":{"type":"object","properties":{"b":{"type":"string"},"a":{"type":["string","null"]}}},"A":{"type":"string"}}}}"#;
        let doc: Json = serde_json::from_str(text).expect("JSON");
        let out = serde_json::to_string(&downgrade(&doc).expect("downgrades")).expect("JSON");
        assert_eq!(
            out,
            r#"{"openapi":"3.0.3","info":{"title":"t","version":"1"},"paths":{},"components":{"schemas":{"Z":{"type":"object","properties":{"b":{"type":"string"},"a":{"type":"string","nullable":true}}},"A":{"type":"string"}}}}"#
        );
    }

    #[test]
    fn a_nullable_type_list_becomes_nullable() {
        assert_eq!(
            translate(json!({"type": ["string", "null"], "format": "date-time"})),
            Ok(tree(
                json!({"type": "string", "format": "date-time", "nullable": true})
            ))
        );
    }

    #[test]
    fn a_nullable_reference_is_replaced_by_a_copy_of_the_component() {
        assert_eq!(
            translate_beside(
                json!({"oneOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}], "description": "outer"}),
                json!({"A": {"type": "object", "description": "inner", "properties": {"x": {"type": ["integer", "null"]}}}})
            ),
            Ok(tree(json!({
                "description": "outer",
                "type": "object",
                "properties": {"x": {"type": "integer", "nullable": true}},
                "nullable": true
            })))
        );
    }

    #[test]
    fn a_nullable_enum_reference_lists_null() {
        assert_eq!(
            translate_beside(
                json!({"oneOf": [{"$ref": "#/components/schemas/E"}, {"type": "null"}]}),
                json!({"E": {"type": "string", "enum": ["a", "b"]}})
            ),
            Ok(tree(
                json!({"type": "string", "enum": ["a", "b", null], "nullable": true})
            ))
        );
    }

    #[test]
    fn a_self_referencing_nullable_component_is_inlined_once() {
        let node = json!({
            "type": "object",
            "properties": {"parent": {"oneOf": [{"$ref": "#/components/schemas/N"}, {"type": "null"}]}}
        });
        let out = translate_beside(
            json!({"oneOf": [{"$ref": "#/components/schemas/N"}, {"type": "null"}]}),
            json!({"N": node}),
        )
        .expect("downgrades");
        assert_eq!(
            out,
            tree(json!({
                "type": "object",
                "properties": {"parent": {"allOf": [{"$ref": "#/components/schemas/N"}], "nullable": true}},
                "nullable": true
            }))
        );
    }

    #[test]
    fn an_unknown_nullable_reference_becomes_a_nullable_all_of_with_its_siblings_lifted() {
        assert_eq!(
            translate(
                json!({"oneOf": [{"$ref": "#/components/schemas/Missing", "description": "d"}, {"type": "null"}]})
            ),
            Ok(tree(
                json!({"allOf": [{"$ref": "#/components/schemas/Missing"}], "description": "d", "nullable": true})
            ))
        );
    }

    #[test]
    fn a_null_branch_beside_an_inline_schema_is_folded_in() {
        assert_eq!(
            translate(
                json!({"anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}], "description": "d"})
            ),
            Ok(tree(
                json!({"description": "d", "type": "integer", "minimum": 0, "nullable": true})
            ))
        );
    }

    #[test]
    fn several_non_null_branches_stay_a_one_of() {
        assert_eq!(
            translate(
                json!({"oneOf": [{"type": "integer"}, {"type": "string"}, {"type": "null"}]})
            ),
            Ok(tree(
                json!({"oneOf": [{"type": "integer"}, {"type": "string"}], "nullable": true})
            ))
        );
    }

    #[test]
    fn reference_siblings_move_beside_an_all_of() {
        assert_eq!(
            translate(json!({"$ref": "#/components/schemas/A", "description": "d"})),
            Ok(tree(
                json!({"allOf": [{"$ref": "#/components/schemas/A"}], "description": "d"})
            ))
        );
        assert_eq!(
            translate(json!({"$ref": "#/components/schemas/A"})),
            Ok(tree(json!({"$ref": "#/components/schemas/A"})))
        );
    }

    #[test]
    fn const_examples_and_numeric_exclusive_bounds_are_restated() {
        assert_eq!(
            translate(json!({"type": "string", "const": "all", "examples": ["all", "x"]})),
            Ok(tree(
                json!({"type": "string", "enum": ["all"], "example": "all"})
            ))
        );
        assert_eq!(
            translate(json!({"type": "number", "exclusiveMinimum": 0})),
            Ok(tree(
                json!({"type": "number", "minimum": 0, "exclusiveMinimum": true})
            ))
        );
    }

    #[test]
    fn string_property_names_are_dropped_and_others_refused() {
        assert_eq!(
            translate(
                json!({"type": "object", "propertyNames": {"type": "string"}, "additionalProperties": {"type": ["integer", "null"]}})
            ),
            Ok(tree(
                json!({"type": "object", "additionalProperties": {"type": "integer", "nullable": true}})
            ))
        );
        assert_eq!(
            translate(
                json!({"type": "object", "propertyNames": {"type": "string", "pattern": "^a"}})
            ),
            Err(unsupported("/components/schemas/S", "propertyNames"))
        );
    }

    #[test]
    fn nested_schemas_are_translated() {
        assert_eq!(
            translate(json!({
                "type": "object",
                "properties": {"a": {"type": "array", "items": {"type": ["string", "null"]}}},
                "not": {"type": ["boolean", "null"]}
            })),
            Ok(tree(json!({
                "type": "object",
                "properties": {"a": {"type": "array", "items": {"type": "string", "nullable": true}}},
                "not": {"type": "boolean", "nullable": true}
            })))
        );
    }

    #[test]
    fn schemas_under_operations_are_translated_and_examples_are_kept() {
        let doc = tree(json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {"/a": {"get": {
                "parameters": [{"name": "q", "in": "query", "schema": {"type": ["integer", "null"]}}],
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"type": ["string", "null"]},
                    "example": {"schema": {"type": ["string", "null"]}}
                }}}}
            }}}
        }));
        let out = serde_json::to_value(downgrade(&doc).expect("downgrades")).expect("JSON");
        let get = &out["paths"]["/a"]["get"];
        assert_eq!(
            get["parameters"][0]["schema"],
            json!({"type": "integer", "nullable": true})
        );
        let media = &get["responses"]["200"]["content"]["application/json"];
        assert_eq!(media["schema"], json!({"type": "string", "nullable": true}));
        assert_eq!(
            media["example"],
            json!({"schema": {"type": ["string", "null"]}})
        );
    }

    #[test]
    fn forms_without_a_3_0_equivalent_are_refused() {
        assert_eq!(
            translate(json!({"type": "array", "prefixItems": [{"type": "string"}]})),
            Err(unsupported("/components/schemas/S", "prefixItems"))
        );
        assert_eq!(
            translate(json!({"type": ["string", "integer"]})),
            Err(shape(
                "/components/schemas/S/type",
                "exactly one non-null type"
            ))
        );
        assert_eq!(
            translate(json!({"type": "null"})),
            Err(shape("/components/schemas/S/type", "a 3.0 type name"))
        );
        assert_eq!(
            translate(json!(true)),
            Err(shape("/components/schemas/S", "a schema object"))
        );
        let mut doc =
            serde_json::to_value(doc_with(json!({"S": {"type": "string"}}))).expect("JSON");
        doc["webhooks"] = json!({});
        assert_eq!(downgrade(&tree(doc)), Err(unsupported("", "webhooks")));
    }

    #[test]
    fn pointers_escape_slashes_and_tildes() {
        let doc = tree(json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {"/a~b": {"get": {"responses": {"200": {"description": "ok", "content": {
                "application/json": {"schema": {"contentMediaType": "x"}}
            }}}}}}
        }));
        assert_eq!(
            downgrade(&doc),
            Err(unsupported(
                "/paths/~1a~0b/get/responses/200/content/application~1json/schema",
                "contentMediaType"
            ))
        );
    }
}

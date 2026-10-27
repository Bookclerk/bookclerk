//! The daemon's OpenAPI document restated as OpenAPI 3.0.3.
//!
//! [`super::document`] is OpenAPI 3.1, which is what utoipa writes and what
//! viewers such as Scalar read. Tools that only understand 3.0 semantics
//! (Cloudflare API Shield's schema validation among them) misread the 3.1
//! forms the document uses, so the daemon also serves this translation at
//! [`OPENAPI_V30_PATH`]. The translation is sans-IO: [`downgrade`] maps one
//! JSON document to another and fails, naming the place, on any 3.1 form it
//! has no 3.0 equivalent for, so a new construct cannot slip through
//! silently.
//!
//! What changes:
//!
//! - `type: [T, "null"]` becomes `type: T` with `nullable: true`.
//! - A `oneOf` / `anyOf` with a `{"type": "null"}` branch drops that branch
//!   and sets `nullable: true`; a single remaining branch is folded in.
//! - A `$ref` with sibling keys (3.0 ignores siblings) moves into a
//!   one-item `allOf` so the siblings, such as `description`, still apply.
//! - `const` becomes a one-value `enum`, `examples` becomes `example` (the
//!   first one), and numeric `exclusiveMinimum` / `exclusiveMaximum` become
//!   `minimum` / `maximum` with the boolean flag.
//! - `propertyNames` has no 3.0 form and is dropped; the map's
//!   `description` still states what its keys are.

use std::fmt;

use serde_json::{Map, Value};

/// Path the 3.0.3 document is served at.
pub const OPENAPI_V30_PATH: &str = "/api/openapi-3.0.json";

/// The OpenAPI version the translation writes.
const VERSION: &str = "3.0.3";

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

/// Restates an OpenAPI 3.1 document as OpenAPI 3.0.3.
///
/// # Arguments
///
/// * `doc` - The 3.1 document as JSON.
///
/// # Returns
///
/// The 3.0.3 document: the same paths, operations, components and texts,
/// with every schema in 3.0 form.
///
/// # Errors
///
/// [`DowngradeError`] when the document uses a 3.1 form without a 3.0
/// equivalent (`webhooks`, `prefixItems`, a `type` list of two non-null
/// types, …) or a schema is not an object.
pub fn downgrade(doc: &Value) -> Result<Value, DowngradeError> {
    let Value::Object(root) = doc else {
        return Err(shape("", "a JSON object"));
    };
    for keyword in ["webhooks", "jsonSchemaDialect"] {
        if root.contains_key(keyword) {
            return Err(unsupported("", keyword));
        }
    }
    if root.get("info").and_then(|i| i.get("summary")).is_some() {
        return Err(unsupported("/info", "summary"));
    }
    if root
        .get("info")
        .and_then(|i| i.get("license"))
        .and_then(|l| l.get("identifier"))
        .is_some()
    {
        return Err(unsupported("/info/license", "identifier"));
    }
    if root
        .get("components")
        .and_then(|c| c.get("pathItems"))
        .is_some()
    {
        return Err(unsupported("/components", "pathItems"));
    }
    let mut out = Map::new();
    for (key, value) in root {
        let pointer = format!("/{}", escape(key));
        let value = match key.as_str() {
            "openapi" => Value::String(VERSION.to_string()),
            "components" => components(value, &pointer)?,
            _ => non_schema(value, &pointer)?,
        };
        out.insert(key.clone(), value);
    }
    Ok(Value::Object(out))
}

/// Translates the `components` object: its `schemas` map holds schemas, and
/// everything else is walked for nested `schema` fields.
///
/// # Arguments
///
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
fn components(value: &Value, pointer: &str) -> Result<Value, DowngradeError> {
    let Value::Object(map) = value else {
        return Err(shape(pointer, "a JSON object"));
    };
    let mut out = Map::new();
    for (key, inner) in map {
        let at = format!("{pointer}/{}", escape(key));
        let inner = if key == "schemas" {
            schema_map(inner, &at)?
        } else {
            non_schema(inner, &at)?
        };
        out.insert(key.clone(), inner);
    }
    Ok(Value::Object(out))
}

/// Walks a part of the document that is not a schema, translating every
/// value held under a `schema` key.
///
/// `example` and `examples` values are data, not document structure, so
/// they are copied unchanged.
///
/// # Arguments
///
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
fn non_schema(value: &Value, pointer: &str) -> Result<Value, DowngradeError> {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, inner) in map {
                let at = format!("{pointer}/{}", escape(key));
                let inner = match key.as_str() {
                    "schema" => schema(inner, &at)?,
                    "example" | "examples" => inner.clone(),
                    _ => non_schema(inner, &at)?,
                };
                out.insert(key.clone(), inner);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| non_schema(item, &format!("{pointer}/{i}")))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Translates a map whose values are all schemas (`properties`,
/// `components.schemas`).
///
/// # Arguments
///
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
fn schema_map(value: &Value, pointer: &str) -> Result<Value, DowngradeError> {
    let Value::Object(map) = value else {
        return Err(shape(pointer, "a map of schemas"));
    };
    let mut out = Map::new();
    for (key, inner) in map {
        out.insert(
            key.clone(),
            schema(inner, &format!("{pointer}/{}", escape(key)))?,
        );
    }
    Ok(Value::Object(out))
}

/// Translates a list of schemas (`allOf`, `oneOf`, `anyOf`).
///
/// # Arguments
///
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
fn schema_list(value: &Value, pointer: &str) -> Result<Value, DowngradeError> {
    let Value::Array(items) = value else {
        return Err(shape(pointer, "a list of schemas"));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| schema(item, &format!("{pointer}/{i}")))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// Whether a schema is exactly `{"type": "null"}`.
fn is_null_schema(value: &Value) -> bool {
    value.as_object().is_some_and(|map| {
        map.len() == 1 && map.get("type").and_then(Value::as_str) == Some("null")
    })
}

/// Translates one schema and everything nested in it.
///
/// # Arguments
///
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
fn schema(value: &Value, pointer: &str) -> Result<Value, DowngradeError> {
    let Value::Object(input) = value else {
        return Err(shape(pointer, "a schema object"));
    };
    let mut map = input.clone();
    let mut nullable = false;

    // Null branches first: `{"type": "null"}` has no 3.0 form of its own.
    for keyword in ["oneOf", "anyOf"] {
        let Some(Value::Array(branches)) = map.get(keyword) else {
            continue;
        };
        if !branches.iter().any(is_null_schema) {
            continue;
        }
        nullable = true;
        let rest: Vec<Value> = branches
            .iter()
            .filter(|b| !is_null_schema(b))
            .cloned()
            .collect();
        match rest.as_slice() {
            [] => return Err(shape(pointer, "a non-null branch")),
            [only] => {
                map.remove(keyword);
                let only_is_ref = only.get("$ref").is_some();
                match only {
                    Value::Object(branch) if !only_is_ref => {
                        for (key, inner) in branch {
                            if map.contains_key(key) {
                                return Err(unsupported(pointer, key));
                            }
                            map.insert(key.clone(), inner.clone());
                        }
                    }
                    _ => {
                        if map.contains_key("allOf") {
                            return Err(unsupported(pointer, "allOf"));
                        }
                        map.insert("allOf".to_string(), Value::Array(vec![only.clone()]));
                    }
                }
            }
            _ => {
                map.insert(keyword.to_string(), Value::Array(rest));
            }
        }
    }

    if let Some(types) = map.get("type").cloned() {
        match types {
            Value::String(t) if TYPES_V30.contains(&t.as_str()) => {}
            Value::Array(list) => {
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
                        map.insert("type".to_string(), Value::String(only.clone()));
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

    if let Some(value) = map.remove("const") {
        if map.contains_key("enum") {
            return Err(unsupported(pointer, "const"));
        }
        map.insert("enum".to_string(), Value::Array(vec![value]));
    }
    if let Some(examples) = map.remove("examples") {
        let Value::Array(examples) = examples else {
            return Err(shape(&format!("{pointer}/examples"), "a list of examples"));
        };
        if let Some(first) = examples.into_iter().next() {
            map.entry("example").or_insert(first);
        }
    }
    for (exclusive, bound) in [
        ("exclusiveMinimum", "minimum"),
        ("exclusiveMaximum", "maximum"),
    ] {
        if let Some(Value::Number(limit)) = map.get(exclusive).cloned() {
            if map.contains_key(bound) {
                return Err(unsupported(pointer, exclusive));
            }
            map.insert(bound.to_string(), Value::Number(limit));
            map.insert(exclusive.to_string(), Value::Bool(true));
        }
    }
    map.remove("propertyNames");

    if nullable {
        map.insert("nullable".to_string(), Value::Bool(true));
    }

    // A 3.0 Reference Object ignores siblings, so keep them beside an
    // `allOf` that holds the reference.
    if map.len() > 1 {
        if let Some(reference) = map.remove("$ref") {
            if map.contains_key("allOf") {
                return Err(unsupported(pointer, "$ref"));
            }
            let mut only = Map::new();
            only.insert("$ref".to_string(), reference);
            map.insert("allOf".to_string(), Value::Array(vec![Value::Object(only)]));
        }
    }

    let mut out = Map::new();
    for (key, inner) in map {
        let at = format!("{pointer}/{}", escape(&key));
        let inner = match key.as_str() {
            "$ref" => inner,
            "properties" => schema_map(&inner, &at)?,
            "items" | "not" => schema(&inner, &at)?,
            "additionalProperties" => match inner {
                Value::Bool(_) => inner,
                other => schema(&other, &at)?,
            },
            "allOf" | "oneOf" | "anyOf" => schema_list(&inner, &at)?,
            k if k.starts_with("x-") || SCHEMA_KEYWORDS_V30.contains(&k) => inner,
            _ => return Err(unsupported(pointer, &key)),
        };
        out.insert(key, inner);
    }
    Ok(Value::Object(out))
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

    /// Wraps a schema in a minimal document under `components.schemas.S`.
    fn doc_with(schema: Value) -> Value {
        json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {},
            "components": {"schemas": {"S": schema}}
        })
    }

    /// Translates one schema through [`downgrade`].
    fn translate(schema: Value) -> Result<Value, DowngradeError> {
        downgrade(&doc_with(schema)).map(|doc| doc["components"]["schemas"]["S"].clone())
    }

    #[test]
    fn the_version_is_3_0_3() {
        let doc = downgrade(&doc_with(json!({"type": "string"}))).expect("downgrades");
        assert_eq!(doc["openapi"], "3.0.3");
    }

    #[test]
    fn a_nullable_type_list_becomes_nullable() {
        assert_eq!(
            translate(json!({"type": ["string", "null"], "format": "date-time"})),
            Ok(json!({"type": "string", "format": "date-time", "nullable": true}))
        );
    }

    #[test]
    fn a_null_branch_beside_a_reference_becomes_a_nullable_all_of() {
        assert_eq!(
            translate(json!({"oneOf": [{"$ref": "#/components/schemas/A"}, {"type": "null"}]})),
            Ok(json!({"allOf": [{"$ref": "#/components/schemas/A"}], "nullable": true}))
        );
    }

    #[test]
    fn a_null_branch_beside_an_inline_schema_is_folded_in() {
        assert_eq!(
            translate(
                json!({"anyOf": [{"type": "integer", "minimum": 0}, {"type": "null"}], "description": "d"})
            ),
            Ok(json!({"type": "integer", "minimum": 0, "description": "d", "nullable": true}))
        );
    }

    #[test]
    fn several_non_null_branches_stay_a_one_of() {
        assert_eq!(
            translate(
                json!({"oneOf": [{"type": "integer"}, {"type": "string"}, {"type": "null"}]})
            ),
            Ok(json!({"oneOf": [{"type": "integer"}, {"type": "string"}], "nullable": true}))
        );
    }

    #[test]
    fn reference_siblings_move_beside_an_all_of() {
        assert_eq!(
            translate(json!({"$ref": "#/components/schemas/A", "description": "d"})),
            Ok(json!({"allOf": [{"$ref": "#/components/schemas/A"}], "description": "d"}))
        );
        assert_eq!(
            translate(json!({"$ref": "#/components/schemas/A"})),
            Ok(json!({"$ref": "#/components/schemas/A"}))
        );
    }

    #[test]
    fn const_examples_and_numeric_exclusive_bounds_are_restated() {
        assert_eq!(
            translate(json!({"type": "string", "const": "all", "examples": ["all", "x"]})),
            Ok(json!({"type": "string", "enum": ["all"], "example": "all"}))
        );
        assert_eq!(
            translate(json!({"type": "number", "exclusiveMinimum": 0})),
            Ok(json!({"type": "number", "minimum": 0, "exclusiveMinimum": true}))
        );
    }

    #[test]
    fn property_names_are_dropped() {
        assert_eq!(
            translate(
                json!({"type": "object", "propertyNames": {"type": "string"}, "additionalProperties": {"type": ["integer", "null"]}})
            ),
            Ok(
                json!({"type": "object", "additionalProperties": {"type": "integer", "nullable": true}})
            )
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
            Ok(json!({
                "type": "object",
                "properties": {"a": {"type": "array", "items": {"type": "string", "nullable": true}}},
                "not": {"type": "boolean", "nullable": true}
            }))
        );
    }

    #[test]
    fn schemas_under_operations_are_translated_and_examples_are_kept() {
        let doc = json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {"/a": {"get": {
                "parameters": [{"name": "q", "in": "query", "schema": {"type": ["integer", "null"]}}],
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"type": ["string", "null"]},
                    "example": {"schema": {"type": ["string", "null"]}}
                }}}}
            }}}
        });
        let out = downgrade(&doc).expect("downgrades");
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
        let mut doc = doc_with(json!({"type": "string"}));
        doc["webhooks"] = json!({});
        assert_eq!(downgrade(&doc), Err(unsupported("", "webhooks")));
    }

    #[test]
    fn pointers_escape_slashes_and_tildes() {
        let doc = json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": {"/a~b": {"get": {"responses": {"200": {"description": "ok", "content": {
                "application/json": {"schema": {"contentMediaType": "x"}}
            }}}}}}
        });
        assert_eq!(
            downgrade(&doc),
            Err(unsupported(
                "/paths/~1a~0b/get/responses/200/content/application~1json/schema",
                "contentMediaType"
            ))
        );
    }
}

//! Reference-conformant argument schemas for tool specifications.
//!
//! The Python reference publishes `tools[].function.parameters` straight from
//! Pydantic's `model_json_schema()`. That output has properties this crate must
//! reproduce exactly, because a model prompted for reference behavior reads
//! them: `required` and `additionalProperties` appear only when the reference
//! model actually declares them, optional properties carry a `default`,
//! nullable properties use `anyOf` with an explicit null branch rather than an
//! array-form `type`, and nested models live under `$defs` behind a `$ref`.
//!
//! Building schemas through [`ObjectSchema`] and [`Property`] makes those rules
//! a property of the construction path instead of a per-tool review item.

use serde_json::{Map, Value};

/// A Pydantic-shaped object schema.
///
/// `type` and `properties` are always emitted. `required`, `$defs` and
/// `additionalProperties` are emitted only when the tool actually declares
/// them, matching what `model_json_schema()` produces for a model that leaves
/// `extra` at its default.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct ObjectSchema {
    properties: Map<String, Value>,
    required: Vec<String>,
    defs: Map<String, Value>,
    forbid_extra: bool,
}

impl ObjectSchema {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares a property listed in `required`.
    pub fn required(mut self, name: impl Into<String>, property: impl Into<Value>) -> Self {
        let name = name.into();
        self.required.push(name.clone());
        self.properties.insert(name, property.into());
        self
    }

    /// Declares a property absent from `required`.
    pub fn optional(mut self, name: impl Into<String>, property: impl Into<Value>) -> Self {
        self.properties.insert(name.into(), property.into());
        self
    }

    /// Registers a `$defs` entry referenced by [`Property::reference`].
    ///
    /// Definitions nested inside `definition` are hoisted to this schema's
    /// `$defs`, the way Pydantic flattens every nested model to the root.
    pub fn define(mut self, name: impl Into<String>, definition: impl Into<Value>) -> Self {
        let mut definition = definition.into();
        if let Some(object) = definition.as_object_mut()
            && let Some(Value::Object(nested)) = object.remove("$defs")
        {
            self.defs.extend(nested);
        }
        self.defs.insert(name.into(), definition);
        self
    }

    /// Mirrors a reference model declaring `extra="forbid"`.
    pub fn forbid_extra_properties(mut self) -> Self {
        self.forbid_extra = true;
        self
    }

    #[must_use]
    pub fn build(self) -> Value {
        let mut schema = Map::new();
        if !self.defs.is_empty() {
            schema.insert("$defs".to_owned(), Value::Object(self.defs));
        }
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(self.properties));
        if !self.required.is_empty() {
            schema.insert(
                "required".to_owned(),
                Value::Array(self.required.into_iter().map(Value::String).collect()),
            );
        }
        if self.forbid_extra {
            schema.insert("additionalProperties".to_owned(), Value::Bool(false));
        }
        Value::Object(schema)
    }
}

impl From<ObjectSchema> for Value {
    fn from(schema: ObjectSchema) -> Self {
        schema.build()
    }
}

/// One property of an [`ObjectSchema`].
///
/// Constraints (`minimum`, `minItems`, `enum`, ...) belong to the type they
/// constrain, so apply [`Property::constrained`] before [`Property::nullable`]:
/// the null branch must stay unconstrained, exactly as Pydantic emits it.
#[derive(Debug, Clone)]
#[must_use]
pub struct Property(Map<String, Value>);

impl Property {
    pub fn string() -> Self {
        Self::of_type("string")
    }

    pub fn integer() -> Self {
        Self::of_type("integer")
    }

    pub fn number() -> Self {
        Self::of_type("number")
    }

    pub fn boolean() -> Self {
        Self::of_type("boolean")
    }

    /// An open object, as Pydantic emits for a `dict[str, T]` field: the value
    /// schema lands under `additionalProperties` rather than in `properties`.
    pub fn map(values: impl Into<Value>) -> Self {
        let mut schema = Map::new();
        schema.insert("additionalProperties".to_owned(), values.into());
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        Self(schema)
    }

    pub fn array(items: impl Into<Value>) -> Self {
        let mut schema = Map::new();
        schema.insert("items".to_owned(), items.into());
        schema.insert("type".to_owned(), Value::String("array".to_owned()));
        Self(schema)
    }

    /// A `$ref` at an [`ObjectSchema::define`] entry.
    pub fn reference(definition: &str) -> Self {
        let mut schema = Map::new();
        schema.insert(
            "$ref".to_owned(),
            Value::String(format!("#/$defs/{definition}")),
        );
        Self(schema)
    }

    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.0
            .insert("description".to_owned(), Value::String(description.into()));
        self
    }

    /// Marks the property optional with the value the reference applies when
    /// the model omits it.
    pub fn with_default(mut self, default: impl Into<Value>) -> Self {
        self.0.insert("default".to_owned(), default.into());
        self
    }

    /// Adds one JSON Schema keyword (`minimum`, `minItems`, `enum`, ...).
    pub fn constrained(mut self, keyword: impl Into<String>, value: impl Into<Value>) -> Self {
        self.0.insert(keyword.into(), value.into());
        self
    }

    /// Wraps the declared type in `anyOf` with an explicit null branch.
    ///
    /// `description` and `default` stay at the property level, where Pydantic
    /// keeps them.
    pub fn nullable(mut self) -> Self {
        let mut outer = Map::new();
        for annotation in ["default", "description"] {
            if let Some(value) = self.0.remove(annotation) {
                outer.insert(annotation.to_owned(), value);
            }
        }
        let mut null = Map::new();
        null.insert("type".to_owned(), Value::String("null".to_owned()));
        outer.insert(
            "anyOf".to_owned(),
            Value::Array(vec![Value::Object(self.0), Value::Object(null)]),
        );
        Self(outer)
    }

    fn of_type(name: &str) -> Self {
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String(name.to_owned()));
        Self(schema)
    }
}

impl From<Property> for Value {
    fn from(property: Property) -> Self {
        Self::Object(property.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_argumentless_schema_omits_required_and_additional_properties() {
        assert_eq!(
            ObjectSchema::new().build(),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn extra_forbidding_models_publish_additional_properties_false() {
        let schema = ObjectSchema::new()
            .required("task", Property::string())
            .optional("agent", Property::string().with_default("explore"))
            .forbid_extra_properties()
            .build();
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string"},
                    "agent": {"type": "string", "default": "explore"},
                },
                "required": ["task"],
                "additionalProperties": false,
            })
        );
    }

    #[test]
    fn nullable_properties_emit_any_of_and_keep_annotations_outside() {
        let schema = ObjectSchema::new()
            .optional(
                "offset",
                Property::integer()
                    .constrained("minimum", 1)
                    .described("start line")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .build();
        assert_eq!(
            schema["properties"]["offset"],
            json!({
                "anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}],
                "default": null,
                "description": "start line",
            })
        );
    }

    #[test]
    fn nested_definitions_are_hoisted_to_the_root_defs() {
        let item = ObjectSchema::new()
            .define(
                "Status",
                Property::string().constrained("enum", json!(["a"])),
            )
            .required("id", Property::string())
            .optional("status", Property::reference("Status").with_default("a"))
            .forbid_extra_properties();
        let schema = ObjectSchema::new()
            .define("Item", item)
            .required("items", Property::array(Property::reference("Item")))
            .build();
        assert_eq!(
            schema["$defs"],
            json!({
                "Status": {"type": "string", "enum": ["a"]},
                "Item": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "status": {"$ref": "#/$defs/Status", "default": "a"},
                    },
                    "required": ["id"],
                    "additionalProperties": false,
                },
            })
        );
        assert_eq!(
            schema["properties"]["items"],
            json!({"type": "array", "items": {"$ref": "#/$defs/Item"}})
        );
    }
}

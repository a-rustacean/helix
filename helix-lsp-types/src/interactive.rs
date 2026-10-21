//! Types for interactive refactoring (LSP proposal #1164).
//!
//! Interactive refactoring parameterizes commands with server-defined forms. The client sends an
//! [`ExecuteCommandParams`](crate::ExecuteCommandParams) to the `command/resolve` request and the
//! server responds with the same params, optionally populated with [`FormField`]s. The client
//! collects answers for those fields and repeats the request, populating `formAnswers`, until the
//! server omits `formFields`. The command is then executed with the resolved params.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

bitflags::bitflags! {
    /// Whether the file denoted by a document URI exists.
    ///
    /// This is a bit set allowing combinations of existence states. For example,
    /// `NEW | EXISTING` allows either state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct FileExistence: u32 {
        /// The file has not yet been created.
        const NEW = 1 << 0;
        /// The file already exists.
        const EXISTING = 1 << 1;
    }
}

impl Serialize for FileExistence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.bits())
    }
}

impl<'de> Deserialize<'de> for FileExistence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bits = u32::deserialize(deserializer)?;
        Ok(Self::from_bits_truncate(bits))
    }
}

bitflags::bitflags! {
    /// The expected filesystem resource type.
    ///
    /// This is a bit set allowing combinations of file types. For example,
    /// `REGULAR | DIRECTORY` allows either type.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct FileType: u32 {
        /// The resource is a regular file.
        const REGULAR = 1 << 0;
        /// The resource is a directory.
        const DIRECTORY = 1 << 1;
    }
}

impl Serialize for FileType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.bits())
    }
}

impl<'de> Deserialize<'de> for FileType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bits = u32::deserialize(deserializer)?;
        Ok(Self::from_bits_truncate(bits))
    }
}

/// Parameters shared by requests that support interactive resolution.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveParams {
    /// Questions and validation errors for previous answers.
    ///
    /// This field is defined by the server. The interactive phase is complete when the server
    /// returns a response where this field is omitted or empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form_fields: Option<Vec<FormField>>,

    /// Answers to the questions in `form_fields`, linked by the field's unique `id`.
    ///
    /// The client must include an answer for every field. For optional fields the user chose not
    /// to answer, the answer's value must be `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub form_answers: Option<Vec<FormAnswer>>,
}

/// A single question in a form and its validation state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    /// Unique identifier for this field, used to link it to an answer.
    pub id: String,

    /// The prompt presented to the user.
    pub description: String,

    /// The data type and validation constraints for the answer.
    #[serde(rename = "type")]
    pub field_type: FormFieldType,

    /// Whether an answer is required for this field.
    #[serde(default)]
    pub required: bool,

    /// An optional initial value for the answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,

    /// A validation message from the server. If empty or absent the current answer is valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The supported form input types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FormFieldKind {
    /// A simple text input.
    String,
    /// A file or directory URI picker.
    File,
    /// A boolean input.
    Bool,
    /// A numeric input.
    Number,
    /// A selection from a static set of options.
    Enum,
    /// A selection from a dynamic set of options queried on demand.
    LazyEnum,
    /// A homogenous list of items.
    List,
}

impl FormFieldKind {
    /// All input types supported by this client implementation.
    pub const SUPPORTED: [FormFieldKind; 5] = [
        FormFieldKind::String,
        FormFieldKind::File,
        FormFieldKind::Bool,
        FormFieldKind::Number,
        FormFieldKind::Enum,
    ];
}

/// The type and validation constraints of a form field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FormFieldType {
    /// A text input.
    String,

    /// An input for a file or directory URI.
    File {
        /// Whether the file must exist, must not exist, or either.
        #[serde(skip_serializing_if = "Option::is_none")]
        existence: Option<FileExistence>,
        /// The expected file type. Defaults to `REGULAR` when omitted.
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        file_type: Option<FileType>,
        /// Allowed file extensions without the leading dot, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<Vec<String>>,
    },

    /// A boolean input.
    Bool,

    /// A numeric input.
    Number,

    /// A selection from a static set of options.
    Enum {
        /// An optional identifier for the enum type.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The allowable options.
        entries: Vec<FormEnumEntry>,
    },

    /// A selection from a large or dynamic set of options.
    LazyEnum {
        /// Identifies the data source on the server.
        source: String,
        /// Opaque static settings for the source.
        #[serde(skip_serializing_if = "Option::is_none")]
        config: Option<Value>,
    },

    /// A homogenous list of items.
    List {
        /// The type of the items in the list.
        element_type: Box<FormFieldType>,
    },
}

/// A single option in an enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormEnumEntry {
    /// The unique identifier sent back to the server when this option is selected.
    pub value: String,
    /// The human-readable label presented to the user.
    pub description: String,
}

/// A single answer to a form field, identified by the field's unique ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormAnswer {
    /// The ID of the field being answered.
    pub id: String,
    /// The user's answer, or `null` if the user provided no answer for an optional field.
    pub value: Value,
}

/// Client capability for interactive resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveResolveClientCapabilities {
    /// The input types the client supports. The presence of this field implies support for
    /// interactive resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_types: Option<Vec<FormFieldKind>>,
}

/// Server capability for interactive resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveResolveOptions {
    /// The kinds of interactive resolutions the server supports.
    ///
    /// The `"command"` kind indicates support for resolving `ExecuteCommandParams` through the
    /// `command/resolve` request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn form_field_serialization() {
        let field = FormField {
            id: "transform".into(),
            description: "transform rule".into(),
            field_type: FormFieldType::Enum {
                name: None,
                entries: vec![FormEnumEntry {
                    value: "camelcase".into(),
                    description: "CamelCase".into(),
                }],
            },
            required: true,
            default: Some(json!("camelcase")),
            error: None,
        };

        assert_eq!(
            serde_json::to_value(&field).unwrap(),
            json!({
                "id": "transform",
                "description": "transform rule",
                "type": {
                    "kind": "enum",
                    "entries": [
                        { "value": "camelcase", "description": "CamelCase" }
                    ]
                },
                "required": true,
                "default": "camelcase",
            })
        );
    }

    #[test]
    fn file_field_serialization() {
        let field = FormField {
            id: "dest".into(),
            description: "destination".into(),
            field_type: FormFieldType::File {
                existence: Some(FileExistence::NEW),
                file_type: Some(FileType::REGULAR | FileType::DIRECTORY),
                filters: Some(vec!["go".into()]),
            },
            required: true,
            default: None,
            error: None,
        };

        assert_eq!(
            serde_json::to_value(&field).unwrap()["type"],
            json!({
                "kind": "file",
                "existence": 1,
                "type": 3,
                "filters": ["go"],
            })
        );
    }

    #[test]
    fn deserialize_lazy_enum_and_list() {
        let lazy: FormFieldType = serde_json::from_value(json!({
            "kind": "lazyEnum",
            "source": "workspace/symbol",
            "config": { "query": "foo" },
        }))
        .unwrap();
        assert!(matches!(lazy, FormFieldType::LazyEnum { .. }));

        let list: FormFieldType = serde_json::from_value(json!({
            "kind": "list",
            "elementType": { "kind": "string" },
        }))
        .unwrap();
        assert!(matches!(list, FormFieldType::List { .. }));
    }

    #[test]
    fn execute_command_params_roundtrip() {
        let params: crate::ExecuteCommandParams = serde_json::from_value(json!({
            "command": "gopls.modify_tags",
            "arguments": [{ "Modification": "add" }],
            "formFields": [{
                "id": "addTags",
                "description": "tags to add",
                "type": { "kind": "string" },
                "required": true,
            }],
            "formAnswers": [{ "id": "addTags", "value": "json" }],
        }))
        .unwrap();

        let fields = params.interactive.form_fields.as_ref().unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].id, "addTags");
        assert_eq!(
            params.interactive.form_answers,
            Some(vec![FormAnswer {
                id: "addTags".into(),
                value: json!("json"),
            }])
        );

        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value["command"], "gopls.modify_tags");
        assert_eq!(value["formAnswers"][0]["value"], "json");
    }
}

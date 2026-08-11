use std::{
    borrow::Borrow,
    collections::{
        BTreeMap,
        HashMap,
    },
    fmt::{
        self,
        Display,
    },
    iter,
    sync::Arc,
};

use errors::ErrorMetadata;
use serde_json::{
    Number,
    Value as JsonValue,
};
use shape_inference::{
    Shape,
    ShapeConfig,
    ShapeCounter,
    ShapeEnum,
};
use value::{
    export::ValueFormat,
    id_v6::DeveloperDocumentId,
    sorting::TotalOrdF64,
    utils::{
        display_map,
        display_sequence,
    },
    ConvexObject,
    ConvexValue,
    FieldName,
    FieldPath,
    IdentifierFieldName,
    Namespace,
    NamespacedTableMapping,
    TableName,
    TableNumber,
};

use super::DocumentSchema;
use crate::{
    document::{
        CREATION_TIME_FIELD,
        ID_FIELD,
    },
    json_schemas,
    virtual_system_mapping::{
        all_tables_number_to_name,
        VirtualSystemMapping,
    },
};

/// Validates that a Convex value has the given type.
///
/// These are used by both schema enforcement and argument validation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Validator {
    Id(TableName),
    Null,
    Float64,
    Int64,
    /// CommitTs at rest should be an `Int64`, but in a pending write can be a
    /// placeholder value. The CommitTs is injected at commit time.
    CommitTs,
    Boolean,
    String,
    Bytes,
    Literal(LiteralValidator),
    Array(Box<Validator>),
    Record(Box<Validator>, Box<Validator>),
    Object(ObjectValidator),
    Union(Vec<Validator>),
    Any,
}

impl Display for Validator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Validator::Id(table_name) => write!(f, "v.id(\"{table_name}\")"),
            Validator::Null => write!(f, "v.null()"),
            Validator::Float64 => write!(f, "v.float64()"),
            Validator::Int64 => write!(f, "v.int64()"),
            Validator::CommitTs => write!(f, "v.commitTs()"),
            Validator::Boolean => write!(f, "v.boolean()"),
            Validator::String => write!(f, "v.string()"),
            Validator::Bytes => write!(f, "v.bytes()"),
            Validator::Literal(literal) => write!(f, "v.literal({literal})"),
            Validator::Array(validator) => write!(f, "v.array({validator})"),
            Validator::Record(keys, values) => write!(f, "v.record({keys}, {values})"),
            Validator::Object(object_validator) => write!(f, "{object_validator}"),
            Validator::Union(validators) => {
                display_sequence(f, ["v.union(", ")"], validators.iter())
            },
            Validator::Any => write!(f, "v.any()"),
        }
    }
}

impl Validator {
    pub fn check_value(
        &self,
        value: &ConvexValue,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> Result<(), ValidationError> {
        let all_tables_number_to_name =
            all_tables_number_to_name(table_mapping, virtual_system_mapping);
        self.check_value_internal(value, &all_tables_number_to_name, ValidationContext::new())
    }

    fn check_value_internal(
        &self,
        value: &ConvexValue,
        all_tables_number_to_name: &impl Fn(TableNumber) -> anyhow::Result<TableName>,
        context: ValidationContext,
    ) -> Result<(), ValidationError> {
        match (self, value) {
            (Validator::Id(validator_table), ConvexValue::String(s)) => {
                if let Ok(id) = DeveloperDocumentId::decode(s)
                    && let Ok(table_name) = all_tables_number_to_name(id.table())
                {
                    if &table_name != validator_table {
                        if table_name.is_system() {
                            let err = ValidationError::SystemTableReference {
                                id,
                                validator_table: validator_table.clone(),
                                context,
                            };
                            return Err(err);
                        } else {
                            let err = ValidationError::TableNamesDoNotMatch {
                                id,
                                found_table_name: table_name,
                                validator_table: validator_table.clone(),
                                context,
                            };
                            return Err(err);
                        }
                    }
                } else {
                    let err = ValidationError::NoMatch {
                        value: value.clone(),
                        validator: self.clone(),
                        context,
                    };
                    return Err(err);
                }
            },
            (Validator::Null, ConvexValue::Null)
            | (Validator::Float64, ConvexValue::Float64(_))
            | (Validator::Int64, ConvexValue::Int64(_))
            | (Validator::CommitTs, ConvexValue::Int64(_))
            | (Validator::Boolean, ConvexValue::Boolean(_))
            | (Validator::String, ConvexValue::String(_))
            | (Validator::Bytes, ConvexValue::Bytes(_)) => return Ok(()),
            (Validator::Literal(literal), value) => {
                let literal_as_value: ConvexValue = literal.clone().into();
                if value != &literal_as_value {
                    return Err(ValidationError::LiteralValuesDoNotMatch {
                        value: value.clone(),
                        literal_validator: literal.clone(),
                        context,
                    });
                }
            },
            (Validator::Array(t), ConvexValue::Array(v)) => {
                for (i, elt) in v.into_iter().enumerate() {
                    t.check_value_internal(
                        elt,
                        all_tables_number_to_name,
                        context.with(ValidationContextSegment::Index(i)),
                    )?;
                }
            },
            (Validator::Record(key_type, value_type), ConvexValue::Object(object)) => {
                for (key, value) in object.iter() {
                    key_type.check_value_internal(
                        &ConvexValue::from(key.clone()),
                        all_tables_number_to_name,
                        context.with(ValidationContextSegment::Keys),
                    )?;
                    value_type.check_value_internal(
                        value,
                        all_tables_number_to_name,
                        context.with(ValidationContextSegment::Values),
                    )?;
                }
            },
            (Validator::Object(object_validator), ConvexValue::Object(object)) => {
                object_validator.check_object(object, all_tables_number_to_name, context)?;
            },
            (Validator::Union(validators), value) => {
                if validators.len() == 1 {
                    return validators[0].check_value_internal(
                        value,
                        all_tables_number_to_name,
                        context,
                    );
                }

                // If every member is an object validator with a literal value
                // for a shared field, dispatch directly to the member whose
                // literal matches the value instead of trying each member in
                // turn. The dispatched member is the only one that can match,
                // so this accepts and rejects the same values as the loop
                // below, though it reports the dispatched member's error.
                if let ConvexValue::Object(object) = value
                    && let Some((discriminator_field, literal_to_member)) =
                        shared_literal_discriminator(validators)
                    && let Some(literal_value) = object.get::<str>(discriminator_field.borrow())
                    && let Some(member_index) = literal_to_member.get(literal_value)
                {
                    return validators[*member_index].check_value_internal(
                        value,
                        all_tables_number_to_name,
                        context,
                    );
                }

                // Collect the errors from each member so we can surface a more
                // useful error than a generic no-match: if any member fails
                // with a missing or extra field error, return the first such
                // error. Otherwise keep the no-match error.
                let mut member_errors = Vec::new();
                for t in validators {
                    match t.check_value_internal(value, all_tables_number_to_name, context.clone())
                    {
                        Ok(()) => return Ok(()),
                        Err(error) => member_errors.push(error),
                    }
                }
                if let Some(error) = member_errors.into_iter().find(|error| {
                    matches!(
                        error,
                        ValidationError::MissingRequiredField { .. }
                            | ValidationError::ExtraField { .. }
                    )
                }) {
                    return Err(error);
                }
                return Err(ValidationError::NoMatch {
                    value: value.clone(),
                    validator: self.clone(),
                    context,
                });
            },
            (Validator::Any, _) => return Ok(()),
            (..) => {
                return Err(ValidationError::NoMatch {
                    value: value.clone(),
                    validator: self.clone(),
                    context,
                })
            },
        };
        Ok(())
    }

    pub fn from_shape<C: ShapeConfig, S: ShapeCounter>(
        t: &Shape<C, S>,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> Self {
        match t.variant() {
            ShapeEnum::Never => Self::Union(vec![]),
            ShapeEnum::Null => Self::Null,
            ShapeEnum::Int64 => Self::Int64,
            ShapeEnum::Float64 => Self::Float64,
            ShapeEnum::NegativeInf => Self::Float64,
            ShapeEnum::PositiveInf => Self::Float64,
            ShapeEnum::NegativeZero => Self::Float64,
            ShapeEnum::NaN => Self::Float64,
            ShapeEnum::NormalFloat64 => Self::Float64,
            ShapeEnum::Boolean => Self::Boolean,
            ShapeEnum::StringLiteral(s) => {
                Self::Literal(LiteralValidator::String(s.literal.clone()))
            },
            ShapeEnum::Id(table_number) => {
                match all_tables_number_to_name(table_mapping, virtual_system_mapping)(
                    *table_number,
                ) {
                    Ok(table_name) => Self::Id(table_name),
                    Err(_) => Self::String,
                }
            },
            ShapeEnum::FieldName => Self::String,
            ShapeEnum::String => Self::String,
            ShapeEnum::Bytes => Self::Bytes,
            ShapeEnum::Array(array_type) => Self::Array(Box::new(Self::from_shape(
                array_type.element(),
                table_mapping,
                virtual_system_mapping,
            ))),
            ShapeEnum::Object(object_type) => {
                let object_fields = object_type
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            FieldValidator {
                                validator: Self::from_shape(
                                    &v.value_shape,
                                    table_mapping,
                                    virtual_system_mapping,
                                ),
                                optional: v.optional,
                            },
                        )
                    })
                    .collect();
                Self::Object(ObjectValidator(object_fields))
            },
            ShapeEnum::Record(record_type) => Self::Record(
                Box::new(Self::from_shape(
                    record_type.field(),
                    table_mapping,
                    virtual_system_mapping,
                )),
                Box::new(Self::from_shape(
                    record_type.value(),
                    table_mapping,
                    virtual_system_mapping,
                )),
            ),
            ShapeEnum::Union(union_type) => Self::Union(
                union_type
                    .iter()
                    .map(|t| Self::from_shape(t, table_mapping, virtual_system_mapping))
                    .collect(),
            ),
            ShapeEnum::Unknown => Self::Any,
        }
    }

    /// A validator A is a subset of the validator B iff for every value that
    /// conforms to A, the value also conforms to B.
    ///
    /// This verification is used to know if a full table scan can be skipped
    /// when updating the schema. Hence, false negatives are permissible but
    /// false positives are not.
    pub fn is_subset(&self, superset: &Validator) -> bool {
        match (&self, &superset) {
            // Generic types
            (Validator::Array(left_contents), Validator::Array(right_contents)) => {
                left_contents.is_subset(right_contents)
            },
            (
                Validator::Object(ObjectValidator(left_fields)),
                Validator::Object(ObjectValidator(right_fields)),
            ) => {
                // No field disappears
                left_fields
                    .keys()
                    .all(|left_field_name| right_fields.contains_key(left_field_name))
                    && right_fields.iter().all(|(field, right_validator)| -> bool {
                        match left_fields.get(field) {
                            // Either a non-breaking change…
                            Some(left_validator) => {
                                (!left_validator.optional || right_validator.optional) // no mandatory → optional change
                                    && left_validator
                                        .validator
                                        .is_subset(&right_validator.validator)
                            },
                            // …or a new optional field
                            None => right_validator.optional,
                        }
                    })
            },

            // Identical types
            (v1, v2) if v1 == v2 => true,

            // Types that are subsets of other ones
            (_, Validator::Any)
            | (Validator::Literal(LiteralValidator::String(_)), Validator::String)
            | (Validator::Literal(LiteralValidator::Int64(_)), Validator::Int64)
            // CommitTs and Int64 accept the same set of values.
            | (Validator::CommitTs, Validator::Int64)
            | (Validator::Int64, Validator::CommitTs)
            | (Validator::Literal(LiteralValidator::Int64(_)), Validator::CommitTs)
            | (Validator::Literal(LiteralValidator::Float64(_)), Validator::Float64)
            | (Validator::Literal(LiteralValidator::Boolean(_)), Validator::Boolean)
            | (Validator::Id(_), Validator::String) => true,

            // Unions
            (Validator::Union(left_cases), _) => left_cases
                .iter()
                .all(|left_case| left_case.is_subset(superset)),
            (_, Validator::Union(cases)) => {
                if cases.iter().any(|case| self.is_subset(case)) {
                    true
                } else if let Validator::Boolean = self {
                    // Allow boolean ⊆ true | false
                    Validator::Literal(LiteralValidator::Boolean(true)).is_subset(superset)
                        && Validator::Literal(LiteralValidator::Boolean(false)).is_subset(superset)
                } else {
                    false
                }
            },

            _ => false,
        }
    }

    /// Accepts only string-backed validators: `v.string()`, string
    /// `v.literal("...")`, and `v.union(...)` of those (with arbitrary
    /// nesting). Used to restrict component env-var declarations, since env
    /// var values stay string-backed on the wire and in storage.
    pub fn is_string_like_validator(&self) -> bool {
        match self {
            Validator::String => true,
            Validator::Literal(LiteralValidator::String(_)) => true,
            Validator::Union(cases) => cases.iter().all(|c| c.is_string_like_validator()),
            _ => false,
        }
    }

    /// Is this something like `v.union(v.literal("foo"), v.literal("bar"))`
    /// These need to be treated differently if they are the key type for
    /// Validator::Record
    pub(crate) fn is_string_subtype_with_string_literal(&self) -> bool {
        match self {
            Validator::Id(_)
            | Validator::Null
            | Validator::Float64
            | Validator::Int64
            | Validator::CommitTs
            | Validator::Boolean
            | Validator::String
            | Validator::Bytes
            | Validator::Array(_)
            | Validator::Record(..)
            | Validator::Object(_)
            | Validator::Any => false,
            Validator::Literal(l) => match l {
                LiteralValidator::Float64(_)
                | LiteralValidator::Int64(_)
                | LiteralValidator::Boolean(_) => false,
                LiteralValidator::String(_) => true,
            },
            Validator::Union(unions) => unions
                .iter()
                .any(|v| v.is_string_subtype_with_string_literal()),
        }
    }

    /// Returns `true` when it is sometimes possible to have a field with the
    /// given path on the document if this table definition is enforced, or
    /// `false` when it is never possible.
    pub fn can_contain_field(&self, field_path: &FieldPath) -> bool {
        self._can_contain_field(field_path.fields())
    }

    fn _can_contain_field(&self, field_path_parts: &[IdentifierFieldName]) -> bool {
        let Some(first_part) = field_path_parts.first() else {
            return true;
        };

        match &self {
            Validator::Any => true,
            Validator::Union(cases) => cases
                .iter()
                .any(|case| case._can_contain_field(field_path_parts)),
            Validator::Object(ObjectValidator(fields)) => fields
                .get(first_part)
                .map(|field_validator| {
                    field_validator
                        .validator
                        ._can_contain_field(&field_path_parts[1..])
                })
                .unwrap_or(false),
            _ => false,
        }
    }

    /// Returns true if field_path points to a field where at least one allowed
    /// value for that field is could be Array<Float64>.
    ///
    /// Some weird cases - if any path in field_path is Any, we return true. If
    /// any path is a union and at least one of the unions has a path that
    /// matches our field_path that matches, we return true. If the field path
    /// points to an Array<Any> we also return true.
    pub fn overlaps_with_array_float64(&self, field_path: &FieldPath) -> bool {
        self._overlaps_with_array_float64(field_path.fields())
    }

    fn is_valid_vector_validator(validator: &Validator) -> bool {
        match validator {
            Validator::Array(validator) => {
                matches!(**validator, Validator::Float64 | Validator::Any)
            },
            Validator::Any => true,
            Validator::Union(validators) => validators.iter().any(Self::is_valid_vector_validator),
            _ => false,
        }
    }

    fn _overlaps_with_array_float64(&self, field_path_parts: &[IdentifierFieldName]) -> bool {
        let Some(first_part) = field_path_parts.first() else {
            return Self::is_valid_vector_validator(self);
        };

        match &self {
            Validator::Any => true,
            Validator::Union(cases) => cases
                .iter()
                .any(|case| case._overlaps_with_array_float64(field_path_parts)),
            Validator::Object(ObjectValidator(fields)) => fields
                .get(first_part)
                .map(|field_validator| {
                    field_validator
                        .validator
                        ._overlaps_with_array_float64(&field_path_parts[1..])
                })
                .unwrap_or(true),
            _ => false,
        }
    }

    pub fn ensure_supported_for_streaming_export(&self) -> anyhow::Result<()> {
        match self {
            // Leaf values
            Validator::Id(_)
            | Validator::Null
            | Validator::Float64
            | Validator::Int64
            | Validator::CommitTs
            | Validator::Boolean
            | Validator::String
            | Validator::Bytes
            | Validator::Literal(_)
            // Values that map to `any`
            | Validator::Record(_, _)
            | Validator::Any => Ok(()),
            Validator::Array(element_validator) => {
                element_validator.ensure_supported_for_streaming_export()
            },
            Validator::Object(object_validator) => {
                let fields = &object_validator.0;
                for field_validator in fields.values() {
                    field_validator.validator.ensure_supported_for_streaming_export()?
                }
                Ok(())
            },
            Validator::Union(validators) => {
                let mut num_objects = 0;
                for validator in validators {
                    if matches!(validator, Validator::Object(_)) {
                        num_objects += 1;
                    };
                    validator.ensure_supported_for_streaming_export()?
                };
                if num_objects > 1 {
                    Err(anyhow::anyhow!(ErrorMetadata::bad_request(
                        "UnsupportedSchemaForExport",
                        "Schema contains a union of objects, which is not supported for export"
                    )))
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn to_json_schema(&self, value_format: ValueFormat) -> JsonValue {
        match self {
            Validator::Id(table_name) => json_schemas::id(table_name),
            Validator::Null => json_schemas::null(),
            Validator::Float64 => json_schemas::float64(true, value_format),
            Validator::Int64 => json_schemas::int64(value_format),
            Validator::CommitTs => json_schemas::int64(value_format),
            Validator::Boolean => json_schemas::boolean(),
            Validator::String => json_schemas::string(),
            Validator::Bytes => json_schemas::bytes(value_format),
            Validator::Literal(literal_validator) => match literal_validator {
                LiteralValidator::Float64(_) => json_schemas::float64(true, value_format),
                LiteralValidator::Int64(_) => json_schemas::int64(value_format),
                LiteralValidator::Boolean(_) => json_schemas::boolean(),
                LiteralValidator::String(_) => json_schemas::string(),
            },
            Validator::Array(element_validator) => {
                json_schemas::array(element_validator.to_json_schema(value_format))
            },
            Validator::Record(key_validator, value_validator) => json_schemas::record(
                key_validator.to_string(),
                value_validator.to_json_schema(value_format),
            ),
            Validator::Object(object_validator) => {
                object_validator.to_json_schema(AddTopLevelFields::False, value_format)
            },
            Validator::Union(validators) => {
                let options = validators
                    .iter()
                    .map(|v| v.to_json_schema(value_format))
                    .collect();
                json_schemas::union(options)
            },
            Validator::Any => json_schemas::any(),
        }
    }

    pub fn foreign_keys<'a>(&'a self) -> Box<dyn Iterator<Item = &'a TableName> + 'a> {
        Box::new(iter::from_coroutine(
            #[coroutine]
            move || match self {
                Self::Id(table_name) => yield table_name,
                Self::Object(object) => {
                    for table_name in object.foreign_keys() {
                        yield table_name;
                    }
                },
                Self::Array(item) => {
                    for table_name in item.foreign_keys() {
                        yield table_name;
                    }
                },
                Self::Union(options) => {
                    for table_name in options.iter().flat_map(|option| option.foreign_keys()) {
                        yield table_name;
                    }
                },
                Self::Record(key, value) => {
                    for table_name in key.foreign_keys() {
                        yield table_name;
                    }
                    for table_name in value.foreign_keys() {
                        yield table_name;
                    }
                },
                Self::Any
                | Self::Boolean
                | Self::Bytes
                | Self::String
                | Self::Literal(_)
                | Self::Null
                | Self::Float64
                | Self::Int64
                | Self::CommitTs => {},
            },
        ))
    }

    // Filter out `_id` and `_creationTime` at the top level
    pub fn filter_top_level_system_fields(self) -> Self {
        match self {
            Validator::Id(_)
            | Validator::Null
            | Validator::Float64
            | Validator::Int64
            | Validator::CommitTs
            | Validator::Boolean
            | Validator::String
            | Validator::Bytes
            | Validator::Literal(_)
            | Validator::Array(_)
            | Validator::Record(..)
            | Validator::Any => self,
            Validator::Object(o) => Validator::Object(o.filter_system_fields()),
            Validator::Union(validators) => Validator::Union(
                validators
                    .into_iter()
                    .map(|v| v.filter_top_level_system_fields())
                    .collect(),
            ),
        }
    }
}

impl From<DocumentSchema> for Validator {
    fn from(document_schema: DocumentSchema) -> Self {
        match document_schema {
            DocumentSchema::Any => Validator::Any,
            DocumentSchema::Union(validators) => {
                Validator::Union(validators.into_iter().map(Validator::Object).collect())
            },
        }
    }
}

impl From<Option<DocumentSchema>> for Validator {
    fn from(option: Option<DocumentSchema>) -> Self {
        match option {
            None => Validator::Any,
            Some(document_schema) => document_schema.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationContextSegment {
    Field(IdentifierFieldName),
    Index(usize),
    Keys,
    Values,
}

impl Display for ValidationContextSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationContextSegment::Field(field_name) => write!(f, ".{field_name}"),
            ValidationContextSegment::Index(index) => write!(f, "[{index}]"),
            ValidationContextSegment::Keys => write!(f, ".keys()"),
            ValidationContextSegment::Values => write!(f, ".values()"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ValidationContextSegmentNode {
    segment: ValidationContextSegment,
    parent: Option<Arc<ValidationContextSegmentNode>>,
}

/// The path to the value being validated, built lazily. Context is only ever
/// rendered into a string when an error is constructed, so the success path
/// never pays for string allocation: each nesting level pushes a cheap owned
/// segment onto a shared linked list.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidationContext(Option<Arc<ValidationContextSegmentNode>>);

impl ValidationContext {
    pub fn new() -> Self {
        ValidationContext(None)
    }

    pub fn with(&self, segment: ValidationContextSegment) -> Self {
        ValidationContext(Some(Arc::new(ValidationContextSegmentNode {
            segment,
            parent: self.0.clone(),
        })))
    }
}

impl Display for ValidationContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(context) = &self.0 {
            write!(f, "Path: ")?;
            let mut segments = Vec::new();
            let mut node = Some(context);
            while let Some(current) = node {
                segments.push(&current.segment);
                node = current.parent.as_ref();
            }
            for segment in segments.into_iter().rev() {
                write!(f, "{segment}")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialOrd, PartialEq)]
pub enum LiteralValidator {
    Float64(TotalOrdF64),
    Int64(i64),
    Boolean(bool),
    String(value::ConvexString),
}
impl Display for LiteralValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Attempt to display this with JSON. For the values that can't be
        // printed with JSON, fall back to the general type.
        let string = match self {
            LiteralValidator::Float64(float) => {
                if let Some(json_number) = Number::from_f64(f64::from(float.clone())) {
                    serde_json::to_string(&JsonValue::Number(json_number))
                } else {
                    Ok("<number>".to_string())
                }
            },
            LiteralValidator::Int64(_) => Ok("<bigint>".to_string()),
            LiteralValidator::Boolean(bool) => serde_json::to_string(&JsonValue::Bool(*bool)),
            LiteralValidator::String(string) => {
                serde_json::to_string(&JsonValue::String(string.clone().into()))
            },
        }
        .map_err(|_| fmt::Error)?;
        write!(f, "{string}")
    }
}

impl From<LiteralValidator> for ConvexValue {
    fn from(literal: LiteralValidator) -> Self {
        match literal {
            LiteralValidator::Float64(float) => ConvexValue::Float64(float.into()),
            LiteralValidator::Int64(int) => ConvexValue::Int64(int),
            LiteralValidator::Boolean(bool) => ConvexValue::Boolean(bool),
            LiteralValidator::String(string) => ConvexValue::String(string),
        }
    }
}

impl TryFrom<ConvexValue> for LiteralValidator {
    type Error = anyhow::Error;

    fn try_from(v: ConvexValue) -> anyhow::Result<Self> {
        match v {
            ConvexValue::Float64(f) => Ok(LiteralValidator::Float64(f.into())),
            ConvexValue::Int64(i) => Ok(LiteralValidator::Int64(i)),
            ConvexValue::Boolean(b) => Ok(LiteralValidator::Boolean(b)),
            ConvexValue::String(s) => Ok(LiteralValidator::String(s.to_string().try_into()?)),
            _ => Err(anyhow::anyhow!("Value {v} is not a valid literal.")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectValidator(pub BTreeMap<IdentifierFieldName, FieldValidator>);

#[macro_export]
macro_rules! object_validator {
    ($($field_name:expr => $field_type:expr),* $(,)?) => {
        {
            use $crate::schemas::validator::ObjectValidator;
            use std::collections::BTreeMap;
            #[allow(unused_mut)]
            let mut fields = BTreeMap::new();
            {
                $(fields.insert($field_name.to_string().parse()?, $field_type);)*
            }
            ObjectValidator(fields)
        }
    };
}

impl Display for ObjectValidator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        display_map(f, ["v.object({", "})"], self.0.iter())
    }
}

pub enum AddTopLevelFields {
    True(TableName),
    False,
}

impl ObjectValidator {
    /// Checks that the given object matches this object validator, without
    /// cloning the validator into a `Validator::Object` wrapper.
    pub fn check_object(
        &self,
        object: &ConvexObject,
        all_tables_number_to_name: &impl Fn(TableNumber) -> anyhow::Result<TableName>,
        context: ValidationContext,
    ) -> Result<(), ValidationError> {
        for (field_name, field_type) in &self.0 {
            let maybe_value = object.get::<str>(field_name.borrow());
            if let Some(value) = maybe_value {
                field_type.validator.check_value_internal(
                    value,
                    all_tables_number_to_name,
                    context.with(ValidationContextSegment::Field(field_name.clone())),
                )?
            } else if !field_type.optional {
                return Err(ValidationError::MissingRequiredField {
                    object: object.clone(),
                    field_name: field_name.clone(),
                    object_validator: self.clone(),
                    context,
                });
            }
        }
        for field in object.keys() {
            if !self.0.contains_key::<str>(field.borrow()) {
                return Err(ValidationError::ExtraField {
                    object: object.clone(),
                    field_name: field.clone(),
                    object_validator: self.clone(),
                    context,
                });
            }
        }
        Ok(())
    }

    /// Checks that the given value is an object and matches this object
    /// validator. Equivalent to wrapping this validator in
    /// `Validator::Object` and calling `check_value` on it.
    pub fn check_value(
        &self,
        value: &ConvexValue,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> Result<(), ValidationError> {
        match value {
            ConvexValue::Object(object) => {
                let all_tables_number_to_name =
                    all_tables_number_to_name(table_mapping, virtual_system_mapping);
                self.check_object(object, &all_tables_number_to_name, ValidationContext::new())
            },
            _ => Err(ValidationError::NoMatch {
                value: value.clone(),
                validator: Validator::Object(self.clone()),
                context: ValidationContext::new(),
            }),
        }
    }

    pub fn has_validator_for_system_field(&self) -> bool {
        let fields = &self.0;
        fields.keys().any(|f| f.is_system())
    }

    pub fn filter_system_fields(self) -> Self {
        if !self.has_validator_for_system_field() {
            return self;
        }
        let fields = self.0;
        let filtered_fields = fields.into_iter().filter(|(f, _)| !f.is_system()).collect();
        Self(filtered_fields)
    }

    pub fn to_json_schema(
        &self,
        add_top_level_fields: AddTopLevelFields,
        value_format: ValueFormat,
    ) -> JsonValue {
        let fields = &self.0;
        let mut field_infos: BTreeMap<String, json_schemas::FieldInfo> = fields
            .iter()
            .map(|(field_name, field_validator)| {
                (
                    field_name.to_string(),
                    json_schemas::FieldInfo {
                        schema: field_validator.validator.to_json_schema(value_format),
                        optional: field_validator.optional,
                    },
                )
            })
            .collect();
        if let AddTopLevelFields::True(table_name) = add_top_level_fields {
            field_infos.insert(
                ID_FIELD.to_string(),
                json_schemas::FieldInfo {
                    schema: json_schemas::id(&table_name),
                    optional: false,
                },
            );
            field_infos.insert(
                CREATION_TIME_FIELD.to_string(),
                json_schemas::FieldInfo {
                    schema: json_schemas::float64(false, value_format),
                    optional: false,
                },
            );
        };
        json_schemas::object(field_infos)
    }

    pub fn foreign_keys(&self) -> impl Iterator<Item = &TableName> {
        self.0
            .values()
            .flat_map(|field| field.validator.foreign_keys())
    }
}

/// If every validator in a union is an object validator with a literal
/// validator for the same field, returns that field name and a map from
/// literal value to member index. If two members share a literal, only the
/// first is kept: the union still accepts a value with that literal as long
/// as the first such member matches, so dispatching to it is correct.
fn shared_literal_discriminator(
    validators: &[Validator],
) -> Option<(&IdentifierFieldName, HashMap<ConvexValue, usize>)> {
    let mut common_fields: Option<Vec<&IdentifierFieldName>> = None;
    for validator in validators {
        let Validator::Object(ObjectValidator(fields)) = validator else {
            return None;
        };
        let field_names: Vec<&IdentifierFieldName> = fields.keys().collect();
        common_fields = Some(match common_fields {
            None => field_names,
            Some(previous) => previous
                .into_iter()
                .filter(|field| field_names.contains(field))
                .collect(),
        });
    }
    // Find the first common field whose validator is a literal in every
    // member. Building the map only scans field headers, unlike member checks
    // which walk whole value trees, so it is rebuilt on every check call.
    for field in common_fields? {
        let mut literal_to_member = HashMap::new();
        let mut all_literal = true;
        for (index, validator) in validators.iter().enumerate() {
            let Validator::Object(ObjectValidator(fields)) = validator else {
                unreachable!("checked above");
            };
            match fields.get(field) {
                Some(FieldValidator {
                    validator: Validator::Literal(literal),
                    ..
                }) => {
                    literal_to_member
                        .entry(literal.clone().into())
                        .or_insert(index);
                },
                _ => {
                    all_literal = false;
                    break;
                },
            }
        }
        if all_literal {
            return Some((field, literal_to_member));
        }
    }
    None
}

/// Object fields can be optional.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldValidator {
    pub validator: Validator,
    pub optional: bool,
}

impl FieldValidator {
    pub fn validator(&self) -> &Validator {
        &self.validator
    }

    pub fn required_field_type(validator: Validator) -> Self {
        Self {
            validator,
            optional: false,
        }
    }

    pub fn optional_field_type(validator: Validator) -> Self {
        Self {
            validator,
            optional: true,
        }
    }
}

impl Display for FieldValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.optional {
            write!(f, "v.optional({})", self.validator)
        } else {
            write!(f, "{}", self.validator)
        }
    }
}

#[derive(derive_more::Display, Debug, Clone, PartialEq)]
pub enum ValidationError {
    #[display(
        "Found ID \"{id}\" from table `{found_table_name}`, which does not match the table name \
         in validator `v.id(\"{validator_table}\")`.{context}"
    )]
    TableNamesDoNotMatch {
        id: DeveloperDocumentId,
        found_table_name: TableName,
        validator_table: TableName,
        context: ValidationContext,
    },
    #[display(
        "Found ID \"{id}\" from a system table, which does not match the table name in validator \
         `v.id(\"{validator_table}\")`.{context}"
    )]
    SystemTableReference {
        id: DeveloperDocumentId,
        validator_table: TableName,
        context: ValidationContext,
    },
    #[display(
        "`{value}` does not match literal validator `v.literal({literal_validator})`.{context}"
    )]
    LiteralValuesDoNotMatch {
        value: ConvexValue,
        literal_validator: LiteralValidator,
        context: ValidationContext,
    },
    #[display(
        "Object is missing the required field `{field_name}`. Consider wrapping the field \
         validator in `v.optional(...)` if this is expected.
{context}
Object: {object}
Validator: {object_validator}"
    )]
    MissingRequiredField {
        object: ConvexObject,
        field_name: IdentifierFieldName,
        object_validator: ObjectValidator,
        context: ValidationContext,
    },
    #[display(
        "Object contains extra field `{field_name}` that is not in the validator.
{context}
Object: {object}
Validator: {object_validator}"
    )]
    ExtraField {
        object: ConvexObject,
        field_name: FieldName,
        object_validator: ObjectValidator,
        context: ValidationContext,
    },
    #[display(
        "Value does not match validator.
{context}
Value: {value}
Validator: {validator}"
    )]
    NoMatch {
        value: ConvexValue,
        validator: Validator,
        context: ValidationContext,
    },
}

#[cfg(test)]
mod tests {
    use value::{
        ConvexValue,
        TableMapping,
        TableNamespace,
    };

    use crate::{
        obj,
        schemas::validator::{
            FieldValidator,
            LiteralValidator,
            ObjectValidator,
            ValidationContext,
            ValidationContextSegment,
            ValidationError,
            Validator,
        },
        val,
        virtual_system_mapping::{
            all_tables_number_to_name,
            VirtualSystemMapping,
        },
    };

    fn check(validator: &Validator, value: &ConvexValue) -> Result<(), ValidationError> {
        validator.check_value(
            value,
            &TableMapping::new().namespace(TableNamespace::Global),
            &VirtualSystemMapping::default(),
        )
    }

    fn literal_string(value: &str) -> Validator {
        Validator::Literal(LiteralValidator::String(
            value.try_into().expect("valid string literal"),
        ))
    }

    fn required(validator: Validator) -> FieldValidator {
        FieldValidator::required_field_type(validator)
    }

    fn discriminated_union() -> anyhow::Result<Validator> {
        Ok(Validator::Union(vec![
            Validator::Object(object_validator!(
                "kind" => required(literal_string("circle")),
                "radius" => required(Validator::Float64),
            )),
            Validator::Object(object_validator!(
                "kind" => required(literal_string("square")),
                "side" => required(Validator::Float64),
            )),
        ]))
    }

    #[test]
    fn test_validation_context_renders_path_segments() {
        let context = ValidationContext::new()
            .with(ValidationContextSegment::Field("field".parse().unwrap()))
            .with(ValidationContextSegment::Index(3))
            .with(ValidationContextSegment::Keys)
            .with(ValidationContextSegment::Values)
            .with(ValidationContextSegment::Field("nested".parse().unwrap()));
        assert_eq!(
            context.to_string(),
            "Path: .field[3].keys().values().nested"
        );
        assert_eq!(ValidationContext::new().to_string(), "");
    }

    #[test]
    fn test_object_validator_check_value() -> anyhow::Result<()> {
        let validator = ObjectValidator(
            [
                ("a".parse()?, required(Validator::Int64)),
                (
                    "b".parse()?,
                    FieldValidator::optional_field_type(Validator::String),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let ok_value = val!({ "a" => 5 });
        assert!(validator
            .check_value(
                &ok_value,
                &TableMapping::new().namespace(TableNamespace::Global),
                &VirtualSystemMapping::default(),
            )
            .is_ok());

        let missing_value = val!({});
        let error = validator
            .check_value(
                &missing_value,
                &TableMapping::new().namespace(TableNamespace::Global),
                &VirtualSystemMapping::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ValidationError::MissingRequiredField { .. }
        ));

        let extra_value = val!({ "a" => 5, "c" => 1 });
        let error = validator
            .check_value(
                &extra_value,
                &TableMapping::new().namespace(TableNamespace::Global),
                &VirtualSystemMapping::default(),
            )
            .unwrap_err();
        assert!(matches!(error, ValidationError::ExtraField { .. }));

        let non_object = val!(5);
        let error = validator
            .check_value(
                &non_object,
                &TableMapping::new().namespace(TableNamespace::Global),
                &VirtualSystemMapping::default(),
            )
            .unwrap_err();
        assert!(matches!(error, ValidationError::NoMatch { .. }));
        Ok(())
    }

    #[test]
    fn test_object_validator_check_object_reports_path() -> anyhow::Result<()> {
        let validator = ObjectValidator(
            [(
                "a".parse()?,
                required(Validator::Object(ObjectValidator(
                    [
                        ("b".parse()?, required(Validator::Int64)),
                        (
                            "c".parse()?,
                            required(Validator::Literal(LiteralValidator::String(
                                "x".try_into()?,
                            ))),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                ))),
            )]
            .into_iter()
            .collect(),
        );
        let mapping = all_tables_number_to_name(
            &TableMapping::new().namespace(TableNamespace::Global),
            &VirtualSystemMapping::default(),
        );
        // A missing required field reports the path of the object that is
        // missing it, i.e. without the missing field's own segment.
        let object: value::ConvexObject = obj!("a" => {})?;
        let error = validator
            .check_object(&object, &mapping, ValidationContext::new())
            .unwrap_err();
        assert!(matches!(
            error,
            ValidationError::MissingRequiredField { .. }
        ));
        assert!(error.to_string().contains("Path: .a"));

        // A nested value mismatch reports the full path to the value.
        let object: value::ConvexObject = obj!("a" => { "b" => 5, "c" => "y" })?;
        let error = validator
            .check_object(&object, &mapping, ValidationContext::new())
            .unwrap_err();
        assert!(matches!(
            error,
            ValidationError::LiteralValuesDoNotMatch { .. }
        ));
        assert!(error.to_string().contains("Path: .a.c"));
        Ok(())
    }

    #[test]
    fn test_union_dispatches_on_shared_literal() -> anyhow::Result<()> {
        let union = discriminated_union()?;
        // Matches the "square" member even though the first member fails.
        let value = val!({ "kind" => "square", "side" => 2.0 });
        assert!(check(&union, &value).is_ok());

        let value = val!({ "kind" => "circle", "radius" => 1.0 });
        assert!(check(&union, &value).is_ok());
        Ok(())
    }

    #[test]
    fn test_union_dispatch_reports_dispatched_member_error() -> anyhow::Result<()> {
        let union = Validator::Union(vec![
            Validator::Object(object_validator!(
                "kind" => required(literal_string("circle")),
            )),
            Validator::Object(object_validator!(
                "kind" => required(literal_string("square")),
                "side" => required(literal_string("x")),
            )),
        ]);
        // Dispatches directly to the "square" member, so its error is
        // reported instead of the union's generic NoMatch.
        let value = val!({ "kind" => "square", "side" => "y" });
        let error = check(&union, &value).unwrap_err();
        assert!(matches!(
            error,
            ValidationError::LiteralValuesDoNotMatch { .. }
        ));
        Ok(())
    }

    #[test]
    fn test_union_falls_back_without_discriminator() -> anyhow::Result<()> {
        let union = discriminated_union()?;
        // Not an object: falls back to the loop, which reports NoMatch.
        let value = val!(5);
        let error = check(&union, &value).unwrap_err();
        assert!(matches!(error, ValidationError::NoMatch { .. }));

        // Lacks the discriminator field: falls back to the loop, which
        // reports the first member's missing-field error.
        let value = val!({ "radius" => 1.0 });
        let error = check(&union, &value).unwrap_err();
        assert!(matches!(
            error,
            ValidationError::MissingRequiredField { .. }
        ));
        Ok(())
    }

    #[test]
    fn test_union_reports_first_missing_field_error() -> anyhow::Result<()> {
        let union = Validator::Union(vec![
            Validator::Object(object_validator!(
                "kind" => required(literal_string("circle")),
                "x" => required(Validator::Int64),
            )),
            Validator::String,
        ]);
        let value = val!({ "kind" => "circle" });
        let error = check(&union, &value).unwrap_err();
        match error {
            ValidationError::MissingRequiredField { field_name, .. } => {
                assert_eq!(field_name.to_string(), "x")
            },
            e => panic!("unexpected error: {e}"),
        }
        Ok(())
    }

    #[test]
    fn test_union_reports_no_match_when_no_member_is_close() -> anyhow::Result<()> {
        let union = Validator::Union(vec![Validator::Int64, Validator::Boolean]);
        let value = val!("string");
        let error = check(&union, &value).unwrap_err();
        assert!(matches!(error, ValidationError::NoMatch { .. }));
        Ok(())
    }
}

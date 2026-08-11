use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        LazyLock,
        Mutex,
    },
};

use common::{
    errors::JsError,
    json::JsonForm,
    schemas::validator::{
        ObjectValidator,
        Validator,
    },
    virtual_system_mapping::VirtualSystemMapping,
};
use errors::ErrorMetadataAnyhowExt;
use lru::LruCache;
use serde::{
    Deserialize,
    Serialize,
};
use value::{
    ConvexArray,
    ConvexValue,
    NamespacedTableMapping,
    PendingValue,
    MAX_COMMIT_TS,
};

/// Capacity of the per-process LRU caches for parsed args/returns validators.
/// The validator JSON strings repeat across deployments and across every UDF
/// invocation (the JS runtime passes `exportArgs()`/`exportReturns()` JSON to
/// the `validateArgs`/`validateReturns` ops on each call), so memoizing the
/// parse avoids re-allocating the validator tree on every call.
const VALIDATOR_CACHE_CAPACITY: usize = 1000;

static ARGS_VALIDATOR_CACHE: LazyLock<Mutex<LruCache<String, Arc<ArgsValidator>>>> =
    LazyLock::new(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(VALIDATOR_CACHE_CAPACITY).expect("non-zero cache capacity"),
        ))
    });

static RETURNS_VALIDATOR_CACHE: LazyLock<Mutex<LruCache<String, Arc<ReturnsValidator>>>> =
    LazyLock::new(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(VALIDATOR_CACHE_CAPACITY).expect("non-zero cache capacity"),
        ))
    });

/**
 * A validator for the arguments to a UDF.
 */
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsValidator {
    Unvalidated,
    Validated(ObjectValidator),
}

impl ArgsValidator {
    pub fn check_args(
        &self,
        args: &ConvexArray,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> anyhow::Result<Option<JsError>> {
        let result = match self {
            ArgsValidator::Unvalidated => None,
            ArgsValidator::Validated(object_validator) => {
                let single_arg = match &args[..] {
                    [arg] => arg,
                    _ => {
                        let error_message = format!(
                            "Expected to receive a single object as the function's argument. \
                             Instead received {} arguments: {args}",
                            args.len()
                        );
                        return Ok(Some(JsError::from_message(error_message)));
                    },
                };
                if !matches!(single_arg, ConvexValue::Object(_)) {
                    let error_message = format!(
                        "Expected to receive an object as the function's argument. Instead \
                         received: {single_arg}"
                    );
                    return Ok(Some(JsError::from_message(error_message)));
                }

                let validation_error =
                    object_validator.check_value(single_arg, table_mapping, virtual_system_mapping);
                if let Err(error) = validation_error {
                    Some(JsError::from_message(error.to_string()))
                } else {
                    None
                }
            },
        };
        Ok(result)
    }

    /// Validates pending args by resolving commit timestamps to
    /// `MAX_COMMIT_TS`
    pub fn check_pending_args(
        &self,
        args: Vec<PendingValue>,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> anyhow::Result<Option<JsError>> {
        let projected = args
            .into_iter()
            .map(|arg| arg.into_resolved(MAX_COMMIT_TS))
            .collect::<anyhow::Result<Vec<_>>>()
            .and_then(ConvexArray::try_from)?;
        self.check_args(&projected, table_mapping, virtual_system_mapping)
    }

    /// Parse a JSON-serialized args validator, memoizing on the schema string.
    ///
    /// The JS runtime passes the same `exportArgs()` JSON to the
    /// `validateArgs` op on every UDF invocation, so this avoids re-parsing
    /// the validator on each call. Failures aren't cached so the error path
    /// behaves identically to a direct parse.
    pub fn json_deserialize_cached(json: &str) -> anyhow::Result<Self> {
        if let Some(validator) = ARGS_VALIDATOR_CACHE
            .lock()
            .expect("args validator cache poisoned")
            .get(json)
        {
            return Ok((**validator).clone());
        }
        let validator = Arc::new(Self::json_deserialize(json)?);
        ARGS_VALIDATOR_CACHE
            .lock()
            .expect("args validator cache poisoned")
            .put(json.to_string(), validator.clone());
        Ok((*validator).clone())
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ArgsValidatorJson(<Validator as JsonForm>::Json);

impl JsonForm for ArgsValidator {
    type Json = ArgsValidatorJson;
}

impl TryFrom<ArgsValidatorJson> for ArgsValidator {
    type Error = anyhow::Error;

    fn try_from(json: ArgsValidatorJson) -> Result<Self, Self::Error> {
        let args = match Validator::try_from(json.0).map_err(|e| {
            e.wrap_error_message(|msg| {
                format!("Error in args validator: {msg}\n\
                    See https://docs.convex.dev/functions/validation for \
                    docs on how to do argument validation.")
            })
        })? {
            Validator::Object(o) => ArgsValidator::Validated(o),
            Validator::Any => ArgsValidator::Unvalidated,
            _ => anyhow::bail!("Args validator must be an object or any"),
        };
        Ok(args)
    }
}

impl TryFrom<ArgsValidator> for ArgsValidatorJson {
    type Error = anyhow::Error;

    fn try_from(args: ArgsValidator) -> Result<Self, Self::Error> {
        let validator = match args {
            ArgsValidator::Unvalidated => Validator::Any,
            ArgsValidator::Validated(args_schema) => Validator::Object(args_schema),
        };

        Ok(ArgsValidatorJson(validator.try_into()?))
    }
}

/**
 * A validator for the return value of a UDF.
 */
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnsValidator {
    Unvalidated,
    Validated(Validator),
}

impl ReturnsValidator {
    pub fn needs_validation(&self) -> bool {
        !matches!(
            self,
            ReturnsValidator::Unvalidated | ReturnsValidator::Validated(Validator::Any)
        )
    }

    pub fn check_output(
        &self,
        output: &ConvexValue,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> Option<JsError> {
        match self {
            ReturnsValidator::Unvalidated => None,
            ReturnsValidator::Validated(validator) => {
                let validation_error =
                    validator.check_value(output, table_mapping, virtual_system_mapping);
                match validation_error {
                    Err(error) => Some(JsError::from_message(format!(
                        "ReturnsValidationError: {error}"
                    ))),
                    Ok(()) => None,
                }
            },
        }
    }

    /// Validates pending values by resolving commit timestamps to
    /// `MAX_COMMIT_TS`
    pub fn check_pending_output(
        &self,
        output: &PendingValue,
        table_mapping: &NamespacedTableMapping,
        virtual_system_mapping: &VirtualSystemMapping,
    ) -> anyhow::Result<Option<JsError>> {
        if !self.needs_validation() {
            return Ok(None);
        }
        let projected = output.resolve(MAX_COMMIT_TS)?;
        Ok(self.check_output(&projected, table_mapping, virtual_system_mapping))
    }

    /// Parse a JSON-serialized returns validator, memoizing on the schema
    /// string. See [`ArgsValidator::json_deserialize_cached`].
    pub fn json_deserialize_cached(json: &str) -> anyhow::Result<Self> {
        if let Some(validator) = RETURNS_VALIDATOR_CACHE
            .lock()
            .expect("returns validator cache poisoned")
            .get(json)
        {
            return Ok((**validator).clone());
        }
        let validator = Arc::new(Self::json_deserialize(json)?);
        RETURNS_VALIDATOR_CACHE
            .lock()
            .expect("returns validator cache poisoned")
            .put(json.to_string(), validator.clone());
        Ok((*validator).clone())
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ReturnsValidatorJson(Option<<Validator as JsonForm>::Json>);

impl JsonForm for ReturnsValidator {
    type Json = ReturnsValidatorJson;
}

impl TryFrom<ReturnsValidatorJson> for ReturnsValidator {
    type Error = anyhow::Error;

    fn try_from(json: ReturnsValidatorJson) -> Result<Self, Self::Error> {
        Ok(match json.0 {
            None => ReturnsValidator::Unvalidated,
            Some(v) => ReturnsValidator::Validated(Validator::try_from(v).map_err(|e| {
                e.wrap_error_message(|msg| {
                    format!("Error in returns validator: {msg}\n\
                            See https://docs.convex.dev/functions/validation for \
                            docs on how to do return value validation.")
                })
            })?),
        })
    }
}

impl TryFrom<ReturnsValidator> for ReturnsValidatorJson {
    type Error = anyhow::Error;

    fn try_from(returns: ReturnsValidator) -> Result<Self, Self::Error> {
        match returns {
            ReturnsValidator::Unvalidated => Ok(Self(None)),
            ReturnsValidator::Validated(output_schema) => Ok(Self(Some(output_schema.try_into()?))),
        }
    }
}

#[cfg(test)]
mod tests {
    use common::json::JsonForm;

    use super::{
        ArgsValidator,
        ReturnsValidator,
    };

    const OBJECT_VALIDATOR_JSON: &str = r#"{"type":"object","value":{}}"#;

    #[test]
    fn test_cached_args_validator_matches_direct_parse() {
        let direct = ArgsValidator::json_deserialize(OBJECT_VALIDATOR_JSON).unwrap();
        let cached = ArgsValidator::json_deserialize_cached(OBJECT_VALIDATOR_JSON).unwrap();
        assert_eq!(direct, cached);
        // A second cached parse must produce an equal validator.
        let cached_again = ArgsValidator::json_deserialize_cached(OBJECT_VALIDATOR_JSON).unwrap();
        assert_eq!(cached, cached_again);
    }

    #[test]
    fn test_cached_returns_validator_matches_direct_parse() {
        let direct = ReturnsValidator::json_deserialize(OBJECT_VALIDATOR_JSON).unwrap();
        let cached = ReturnsValidator::json_deserialize_cached(OBJECT_VALIDATOR_JSON).unwrap();
        assert_eq!(direct, cached);
        let cached_again =
            ReturnsValidator::json_deserialize_cached(OBJECT_VALIDATOR_JSON).unwrap();
        assert_eq!(cached, cached_again);
    }

    #[test]
    fn test_cached_validator_errors_are_not_cached() {
        // A failed parse must not poison the cache for subsequent valid parses.
        assert!(ArgsValidator::json_deserialize_cached("not json").is_err());
        assert!(ReturnsValidator::json_deserialize_cached("not json").is_err());
        let args = ArgsValidator::json_deserialize_cached(OBJECT_VALIDATOR_JSON).unwrap();
        assert!(matches!(args, ArgsValidator::Validated(_)));
        let returns = ReturnsValidator::json_deserialize_cached(OBJECT_VALIDATOR_JSON).unwrap();
        assert!(matches!(returns, ReturnsValidator::Validated(_)));
    }

    #[test]
    fn test_cached_validator_unvalidated() {
        // `{"type":"any"}` (or `null` for returns) parses to the Unvalidated
        // variants.
        let args = ArgsValidator::json_deserialize_cached(r#"{"type":"any"}"#).unwrap();
        assert!(matches!(args, ArgsValidator::Unvalidated));
        let returns = ReturnsValidator::json_deserialize_cached("null").unwrap();
        assert!(matches!(returns, ReturnsValidator::Unvalidated));
    }
}

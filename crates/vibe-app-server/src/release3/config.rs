//! The configuration methods: reading the layered document, patching it,
//! writing a batch of fields, and the two settings that have their own methods.
//!
//! The layering itself is `vibe_core::config`; what is here is the boundary
//! shape, the parameter parsing, and the target a write applies to.

use super::*;

impl Release3Service {
    /// The two configuration views `ConfigReadResponse` declares.
    ///
    /// No agent overlay is applied to the published configuration here, so both
    /// views are the same document; the field stays on the wire because a
    /// client renders "changed from the base" from it.
    pub(super) fn config_read(&self) -> Result<Release3Dispatch, Release3Error> {
        let view = self.config.load().map_err(config_error)?.config_view();
        Ok(Release3Dispatch::result([
            ("config", view.clone()),
            ("baseConfig", view),
        ]))
    }

    /// The whole configuration, with every layer and target it was composed
    /// from.
    ///
    /// This is not a wire shape: `ConfigReadResponse` publishes a narrower view
    /// and declares no room for the rest. It stays for the in-process readers
    /// that need the effective document, chiefly the settings screen.
    pub fn config_document(&self) -> Result<Value, Release3Error> {
        Ok(self.config.load().map_err(config_error)?.public_view())
    }

    /// Writes one or more addressed fields, routing each to the file the client
    /// named or, failing that, to the writable target the selection resolves to.
    ///
    /// The response splits the two ways a patch can fail the way
    /// `ConfigPatchResponse` splits them: `rejected` for a request the
    /// merged-configuration preflight refused, which leaves every file
    /// byte-identical, and `failures` for a target whose write did not land
    /// while another one did. The server fills in the runtime the patch
    /// produced, which is what a client reads the new values from.
    ///
    /// `reloadRuntime` is accepted and has no effect: `config/read` and
    /// `config/reload` both compose from disk on every call here, so there is no
    /// cached runtime a patch could leave stale.
    pub(super) fn config_patch(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let raw = params
            .get("ops")
            .and_then(Value::as_array)
            .ok_or_else(|| Release3Error::InvalidParams("ops must be an array".to_owned()))?;
        let operations = raw
            .iter()
            .map(parse_config_patch_op)
            .collect::<Result<Vec<_>, _>>()?;
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("config screen edit");
        let outcome = match self.config.apply_patch(&operations, reason) {
            Ok(outcome) => outcome,
            Err(vibe_core::config::ConfigError::PatchRejected(_)) => {
                return Ok(Release3Dispatch::result([
                    ("rejected", Value::Bool(true)),
                    ("failures", json!([])),
                ]));
            }
            Err(error) => return Err(config_error(error)),
        };
        Ok(Release3Dispatch::result([
            ("rejected", Value::Bool(false)),
            ("failures", json!(outcome.failures)),
        ]))
    }

    /// Describes every published field so a settings screen renders without
    /// hard-coding the surface. Reference `_config_fields_read`.
    pub(super) fn config_fields_read(&self) -> Result<Release3Dispatch, Release3Error> {
        let described = self.config.describe_fields().map_err(config_error)?;
        Ok(Release3Dispatch::result([
            ("fields", json!(described.fields)),
            ("targets", json!(described.targets)),
        ]))
    }

    pub(super) fn config_batch_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let raw = params
            .get("writes")
            .and_then(Value::as_array)
            .ok_or_else(|| Release3Error::InvalidParams("writes must be an array".to_owned()))?;
        let mut writes = raw
            .iter()
            .map(parse_config_write)
            .collect::<Result<Vec<_>, _>>()?;
        // A caller that names no fingerprint means "write on top of what is on
        // disk now" rather than "the file must not exist", which is how the
        // addressed writes read an absent one too. The check still stands: the
        // fingerprint is taken here and compared inside the transaction.
        let snapshot = self.config.load().map_err(config_error)?;
        for write in &mut writes {
            if write.expected_fingerprint.is_none() {
                write.expected_fingerprint =
                    snapshot.fingerprints.get(&write.target).cloned().flatten();
            }
        }
        let snapshot = self.config.batch_write(&writes).map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "snapshot",
            snapshot.public_view(),
        )]))
    }

    /// Writes the thinking level, which the reference addresses by name rather
    /// than through the patch surface.
    ///
    /// The answer carries nothing of its own: the server publishes the runtime
    /// the write produced, which is what `ConfigMutationResponse` declares.
    pub(super) fn thinking_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .ok_or_else(|| Release3Error::InvalidParams("level is required".to_owned()))?;
        if !THINKING_LEVELS.contains(&level) {
            return Err(Release3Error::InvalidParams(format!(
                "level must be one of {}",
                THINKING_LEVELS.join(", ")
            )));
        }
        let target = parse_target(
            params
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("user"),
        )?;
        let snapshot = self.config.load().map_err(config_error)?;
        let expected_fingerprint = params
            .get("expectedFingerprint")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| snapshot.fingerprints.get(&target).cloned().flatten());
        self.config
            .batch_write(&[ConfigWrite {
                target,
                expected_fingerprint,
                mutations: vec![ConfigMutation::set(
                    ["thinking"],
                    TomlValue::String(level.to_owned()),
                )],
            }])
            .map_err(config_error)?;
        Ok(Release3Dispatch::result([] as [(&str, Value); 0]))
    }

    pub(super) fn proxy_read(&self) -> Result<Release3Dispatch, Release3Error> {
        let values = ProxyEnvironmentStore::new(&self.paths.vibe_home)
            .read()
            .map_err(config_error)?;
        Ok(Release3Dispatch::result([(
            "settings",
            json!({
                "values": ProxyKey::ALL.into_iter().map(|key| {
                    (key.as_str().to_owned(), values.get(&key).cloned().map(Value::String).unwrap_or(Value::Null))
                }).collect::<Map<_, _>>(),
                "descriptions": ProxyKey::ALL.into_iter().map(|key| {
                    (key.as_str().to_owned(), json!(key.description()))
                }).collect::<Map<_, _>>(),
            }),
        )]))
    }

    pub(super) fn proxy_write(
        &self,
        params: &BTreeMap<String, Value>,
    ) -> Result<Release3Dispatch, Release3Error> {
        let changes = params
            .get("changes")
            .and_then(Value::as_object)
            .ok_or_else(|| Release3Error::InvalidParams("changes must be an object".to_owned()))?;
        let mut parsed = BTreeMap::new();
        for (key, value) in changes {
            let key = ProxyKey::try_from(key.as_str())
                .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
            let value = match value {
                Value::Null => None,
                Value::String(value) if value.is_empty() => None,
                Value::String(value) if !value.contains(['\n', '\r', '\0']) => Some(value.clone()),
                Value::String(_) => {
                    return Err(Release3Error::InvalidParams(format!(
                        "proxy value for `{}` contains a forbidden control character",
                        key.as_str()
                    )));
                }
                _ => {
                    return Err(Release3Error::InvalidParams(format!(
                        "proxy value for `{}` must be a string or null",
                        key.as_str()
                    )));
                }
            };
            parsed.insert(key, value);
        }
        if !parsed.is_empty() {
            ProxyEnvironmentStore::new(&self.paths.vibe_home)
                .write(&parsed)
                .map_err(config_error)?;
        }
        Ok(Release3Dispatch::result([] as [(&str, Value); 0]))
    }
}

/// Reads one `ConfigPatchOpWire`: a `set` or `remove` verb, a JSON Pointer, the
/// value a `set` carries, and the file a client pinned the operation to.
pub(super) fn parse_config_patch_op(value: &Value) -> Result<ConfigPatchOp, Release3Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Release3Error::InvalidParams("each op must be an object".to_owned()))?;
    let raw_path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Release3Error::InvalidParams("op.path must be a string".to_owned()))?;
    let pointer = JsonPointer::parse(raw_path)
        .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
    let target = object
        .get("targetLayer")
        .and_then(Value::as_str)
        .map(parse_target)
        .transpose()?;
    let operation = match object.get("op").and_then(Value::as_str) {
        Some("set") => {
            let raw = object.get("value").cloned().unwrap_or(Value::Null);
            PatchOperation::Set(
                TomlValue::try_from(raw)
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?,
            )
        }
        Some("remove") => PatchOperation::Remove,
        _ => {
            return Err(Release3Error::InvalidParams(
                "op.op must be set or remove".to_owned(),
            ));
        }
    };
    Ok(ConfigPatchOp {
        mutation: ConfigMutation::new(pointer, operation),
        target,
    })
}

pub(super) fn parse_config_write(value: &Value) -> Result<ConfigWrite, Release3Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Release3Error::InvalidParams("write must be an object".to_owned()))?;
    let target = parse_target(
        object
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| Release3Error::InvalidParams("write.target is required".to_owned()))?,
    )?;
    let expected_fingerprint = object
        .get("expectedFingerprint")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let mutations = object
        .get("mutations")
        .and_then(Value::as_array)
        .ok_or_else(|| Release3Error::InvalidParams("write.mutations must be an array".to_owned()))?
        .iter()
        .map(|mutation| {
            let mutation = mutation.as_object().ok_or_else(|| {
                Release3Error::InvalidParams("mutation must be an object".to_owned())
            })?;
            let path = mutation
                .get("path")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Release3Error::InvalidParams("mutation.path must be an array".to_owned())
                })?
                .iter()
                .map(|part| {
                    part.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        Release3Error::InvalidParams(
                            "mutation path parts must be strings".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if mutation
                .get("remove")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Ok::<ConfigMutation, Release3Error>(ConfigMutation::remove(path))
            } else {
                let raw = mutation.get("value").cloned().ok_or_else(|| {
                    Release3Error::InvalidParams("mutation.value is required".to_owned())
                })?;
                let value = TomlValue::try_from(raw)
                    .map_err(|error| Release3Error::InvalidParams(error.to_string()))?;
                Ok::<ConfigMutation, Release3Error>(ConfigMutation::set(path, value))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ConfigWrite {
        target,
        expected_fingerprint,
        mutations,
    })
}

pub(super) fn parse_target(value: &str) -> Result<ConfigTarget, Release3Error> {
    match value {
        "user" => Ok(ConfigTarget::User),
        "project" => Ok(ConfigTarget::Project),
        _ => Err(Release3Error::InvalidParams(
            "target must be user or project".to_owned(),
        )),
    }
}

pub(super) fn config_map(value: Option<&Value>) -> Result<BTreeMap<String, Value>, Release3Error> {
    value
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Release3Error::Json)
        .map(Option::unwrap_or_default)
}

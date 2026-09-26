//! The six review requests, validated the way the reference's models validate
//! them.
//!
//! Reference `ReviewStateParams`, `ReviewBaselineParams`,
//! `ReviewTurnDiffParams`, `ReviewHunksParams` and `ReviewMutationParams`
//! (`vibe/app_server/protocol.py`), over the owner and target unions of
//! `vibe/app_server/review.py`, all read by `validate_wire`: camelCase names
//! only, unknown fields refused at every level, every violation reported rather
//! than the first, and integers read leniently. A client reads the rejection's
//! `issues` by path, so the paths and the count are the contract; the
//! sentences are this port's own.
//!
//! What pydantic's lax mode takes as an integer is taken here too: a boolean, a
//! float with no fractional part, and a string holding decimal digits (with
//! single underscores between them and an optional all-zero fraction) once its
//! surrounding whitespace is stripped. An integer too large for the type the
//! engine numbers by is not refused, because the reference's are unbounded; it
//! saturates, and a value past every identifier the log hands out names
//! nothing, which is what such a value names upstream too.

use serde_json::{Map, Value};
use vibe_core::checkpoints::{Owner, RegionId, ReviewTarget};
use vibe_protocol::{InvalidParamsIssue, PathSegment};

use crate::params::python_int;

use std::collections::BTreeMap;

/// One validated review request.
pub(crate) struct ReviewRequest {
    /// The session it names.
    pub(crate) session_id: String,
    /// What it asks.
    pub(crate) call: ReviewCall,
}

/// What a review request asks.
pub(crate) enum ReviewCall {
    State,
    Baseline { path: String },
    TurnDiff { path: String, owner: Owner },
    Hunks { path: String, owner: Option<Owner> },
    Approve(ReviewTarget),
    Revert(ReviewTarget),
}

impl ReviewCall {
    /// Whether this call decides something, and so needs an idle session.
    pub(crate) const fn is_mutation(&self) -> bool {
        matches!(self, Self::Approve(_) | Self::Revert(_))
    }
}

/// Validates `params` for `method`, one of the six review methods.
///
/// # Errors
///
/// Answers every violation the reference's model reports, in its order: the
/// declared fields in declaration order, then each unknown field.
pub(crate) fn parse(
    method: &str,
    params: &BTreeMap<String, Value>,
) -> Result<ReviewRequest, Vec<InvalidParamsIssue>> {
    let object: Map<String, Value> = params
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut checker = Checker::default();
    let root = Vec::new();
    let session_id = checker.string(&object, "sessionId", &root);
    let call = match method {
        "review/state" => {
            checker.extras(&object, &["sessionId"], &root);
            Some(ReviewCall::State)
        }
        "review/baseline" => {
            let path = checker.string(&object, "path", &root);
            checker.extras(&object, &["sessionId", "path"], &root);
            path.map(|path| ReviewCall::Baseline { path })
        }
        "review/turnDiff" => {
            let path = checker.string(&object, "path", &root);
            let owner = match object.get("owner") {
                None => {
                    checker.missing(&root, "owner");
                    None
                }
                Some(value) => checker.owner(value, &child(&root, "owner")),
            };
            checker.extras(&object, &["sessionId", "path", "owner"], &root);
            path.zip(owner)
                .map(|(path, owner)| ReviewCall::TurnDiff { path, owner })
        }
        "review/hunks" => {
            let path = checker.string(&object, "path", &root);
            let owner = match object.get("owner") {
                None | Some(Value::Null) => Some(None),
                Some(value) => checker.owner(value, &child(&root, "owner")).map(Some),
            };
            checker.extras(&object, &["sessionId", "path", "owner"], &root);
            path.zip(owner)
                .map(|(path, owner)| ReviewCall::Hunks { path, owner })
        }
        "review/approve" | "review/revert" => {
            let target = match object.get("target") {
                None => {
                    checker.missing(&root, "target");
                    None
                }
                Some(value) => checker.target(value, &child(&root, "target")),
            };
            checker.extras(&object, &["sessionId", "target"], &root);
            target.map(|target| {
                if method == "review/approve" {
                    ReviewCall::Approve(target)
                } else {
                    ReviewCall::Revert(target)
                }
            })
        }
        _ => None,
    };
    match (session_id, call) {
        (Some(session_id), Some(call)) if checker.issues.is_empty() => {
            Ok(ReviewRequest { session_id, call })
        }
        _ => Err(checker.issues),
    }
}

fn child(path: &[PathSegment], field: &str) -> Vec<PathSegment> {
    let mut path = path.to_vec();
    path.push(PathSegment::Field(field.to_owned()));
    path
}

/// The owner union's members, by their `kind` tag.
const OWNER_TAGS: &[&str] = &["agent", "manual"];

/// The target union's members, by their `kind` tag.
const TARGET_TAGS: &[&str] = &[
    "region",
    "regions",
    "scope",
    "scopeFile",
    "file",
    "all",
    "lastTurns",
];

/// Collects the violations of one request.
#[derive(Default)]
struct Checker {
    issues: Vec<InvalidParamsIssue>,
}

impl Checker {
    fn report(&mut self, path: Vec<PathSegment>, message: &str) {
        self.issues.push(InvalidParamsIssue {
            path,
            message: message.to_owned(),
        });
    }

    fn missing(&mut self, parent: &[PathSegment], field: &str) {
        self.report(child(parent, field), "Field required");
    }

    /// Reports every field of `object` that `declared` does not name.
    ///
    /// The envelope holds a request's parameters in key order, so several
    /// unknown fields are reported in that order rather than in the order the
    /// client wrote them.
    fn extras(&mut self, object: &Map<String, Value>, declared: &[&str], parent: &[PathSegment]) {
        for key in object.keys() {
            if !declared.contains(&key.as_str()) {
                self.report(child(parent, key), "Extra inputs are not permitted");
            }
        }
    }

    fn string(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
    ) -> Option<String> {
        match object.get(field) {
            None => {
                self.missing(parent, field);
                None
            }
            Some(Value::String(value)) => Some(value.clone()),
            Some(_) => {
                self.report(child(parent, field), "Input should be a valid string");
                None
            }
        }
    }

    /// An integer field, read leniently, and required unless `ge_zero` is
    /// the only thing wrong with it.
    fn integer(
        &mut self,
        object: &Map<String, Value>,
        field: &str,
        parent: &[PathSegment],
        ge_zero: bool,
    ) -> Option<i128> {
        let path = child(parent, field);
        let Some(value) = object.get(field) else {
            self.missing(parent, field);
            return None;
        };
        match python_int(value) {
            Ok(number) if ge_zero && number < 0 => {
                self.report(path, "Input should be greater than or equal to 0");
                None
            }
            Ok(number) => Some(number),
            Err(message) => {
                self.report(path, message);
                None
            }
        }
    }

    /// The member of a tagged union `value` names, with its fields.
    fn member<'a>(
        &mut self,
        value: &'a Value,
        path: &[PathSegment],
        tags: &[&str],
    ) -> Option<(&'a str, &'a Map<String, Value>)> {
        let Value::Object(object) = value else {
            self.report(
                path.to_vec(),
                "Input should be a valid dictionary or object to extract fields from",
            );
            return None;
        };
        let Some(tag) = object.get("kind") else {
            self.report(
                path.to_vec(),
                "Unable to extract tag using discriminator 'kind'",
            );
            return None;
        };
        match tag.as_str().filter(|tag| tags.contains(tag)) {
            Some(tag) => Some((tag, object)),
            None => {
                self.report(
                    path.to_vec(),
                    "Input tag found using 'kind' does not match any of the expected tags",
                );
                None
            }
        }
    }

    fn owner(&mut self, value: &Value, path: &[PathSegment]) -> Option<Owner> {
        let (tag, object) = self.member(value, path, OWNER_TAGS)?;
        let path = child(path, tag);
        let owner = if tag == "agent" {
            self.integer(object, "turnId", &path, false)
                .map(|turn_id| Owner::Agent {
                    turn_id: identifier(turn_id),
                })
        } else {
            self.integer(object, "index", &path, false)
                .map(|index| Owner::Manual {
                    index: position(index),
                })
        };
        self.extras(
            object,
            &["kind", if tag == "agent" { "turnId" } else { "index" }],
            &path,
        );
        owner
    }

    fn region_ref(&mut self, value: &Value, path: &[PathSegment]) -> Option<RegionId> {
        let Value::Object(object) = value else {
            self.report(
                path.to_vec(),
                "Input should be a valid dictionary or object",
            );
            return None;
        };
        let version_index = self.integer(object, "versionIndex", path, true);
        let ordinal = self.integer(object, "ordinal", path, true);
        self.extras(object, &["versionIndex", "ordinal"], path);
        Some(RegionId::new(
            identifier(version_index?),
            position(ordinal?),
        ))
    }

    fn target(&mut self, value: &Value, path: &[PathSegment]) -> Option<ReviewTarget> {
        let (tag, object) = self.member(value, path, TARGET_TAGS)?;
        let path = child(path, tag);
        let (target, declared): (Option<ReviewTarget>, &[&str]) = match tag {
            "region" => {
                let file = self.string(object, "path", &path);
                let version_index = self.integer(object, "versionIndex", &path, false);
                let ordinal = self.integer(object, "ordinal", &path, false);
                let target = match (file, version_index, ordinal) {
                    (Some(path), Some(version_index), Some(ordinal)) => {
                        Some(ReviewTarget::Region {
                            path,
                            version_index: identifier(version_index),
                            ordinal: position(ordinal),
                        })
                    }
                    _ => None,
                };
                (target, &["kind", "path", "versionIndex", "ordinal"])
            }
            "regions" => {
                let file = self.string(object, "path", &path);
                let regions = match object.get("regions") {
                    None => {
                        self.missing(&path, "regions");
                        None
                    }
                    Some(Value::Array(items)) => {
                        let list = child(&path, "regions");
                        let mut regions = Some(Vec::with_capacity(items.len()));
                        for (index, item) in items.iter().enumerate() {
                            let mut item_path = list.clone();
                            item_path.push(PathSegment::Index(index));
                            match (self.region_ref(item, &item_path), regions.as_mut()) {
                                (Some(region), Some(regions)) => regions.push(region),
                                _ => regions = None,
                            }
                        }
                        regions
                    }
                    Some(_) => {
                        self.report(child(&path, "regions"), "Input should be a valid list");
                        None
                    }
                };
                let target = file
                    .zip(regions)
                    .map(|(path, regions)| ReviewTarget::Regions { path, regions });
                (target, &["kind", "path", "regions"])
            }
            "scope" => {
                let owner = self.owner_field(object, &path);
                (
                    owner.map(|owner| ReviewTarget::Scope { owner }),
                    &["kind", "owner"],
                )
            }
            "scopeFile" => {
                let owner = self.owner_field(object, &path);
                let file = self.string(object, "path", &path);
                let target = owner
                    .zip(file)
                    .map(|(owner, path)| ReviewTarget::ScopeFile { owner, path });
                (target, &["kind", "owner", "path"])
            }
            "file" => {
                let file = self.string(object, "path", &path);
                (
                    file.map(|path| ReviewTarget::File { path }),
                    &["kind", "path"],
                )
            }
            "all" => (Some(ReviewTarget::All), &["kind"]),
            _ => {
                let count = self.integer(object, "count", &path, false);
                let target = count.map(|count| ReviewTarget::LastTurns {
                    count: i64::try_from(count).unwrap_or(if count < 0 {
                        i64::MIN
                    } else {
                        i64::MAX
                    }),
                });
                (target, &["kind", "count"])
            }
        };
        self.extras(object, declared, &path);
        target
    }

    fn owner_field(&mut self, object: &Map<String, Value>, path: &[PathSegment]) -> Option<Owner> {
        match object.get("owner") {
            None => {
                self.missing(path, "owner");
                None
            }
            Some(value) => self.owner(value, &child(path, "owner")),
        }
    }
}

/// A turn or edit identifier from an integer the reference would accept.
///
/// The log numbers both from zero up, so a negative value and one past the
/// type's range both become one no log entry carries.
fn identifier(value: i128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// A slot or ordinal position, on the same terms as [`identifier`].
fn position(value: i128) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

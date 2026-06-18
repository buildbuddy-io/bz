/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bz_build_api::analysis::calculation::RuleAnalysisCalculation;
use bz_build_api::transition::TRANSITION_ATTRS_PROVIDER;
use bz_build_api::transition::TransitionAttrProvider;
use bz_build_api::transition::TransitionAttrs;
use bz_core::configuration::data::BazelBuildSettingValue;
use bz_core::configuration::transition::id::TransitionId;
use bz_core::provider::label::ProvidersLabel;
use bz_interpreter::load_module::InterpreterCalculation;
use dice::DiceComputations;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::ValueSerialize;
use either::Either;
use pagable::Pagable;
use pagable::pagable_typetag;
use ref_cast::RefCast;
use starlark::values::FrozenStringValue;
use starlark::values::OwnedFrozenValueTyped;

use crate::transition::provider::FrozenTransitionInfo;
use crate::transition::starlark::FrozenTransition;

pub(crate) enum TransitionData {
    MagicObject(OwnedFrozenValueTyped<FrozenTransition>),
    BazelAttribute(OwnedFrozenValueTyped<FrozenTransition>),
    AnalysisTest(BTreeMap<String, BazelBuildSettingValue>),
    Target(OwnedFrozenValueTyped<FrozenTransitionInfo>),
}

impl TransitionData {
    pub(crate) fn refs(
        &self,
    ) -> impl Iterator<Item = (&FrozenStringValue, &ProvidersLabel)> + Send + Sync {
        match self {
            TransitionData::MagicObject(v) | TransitionData::BazelAttribute(v) => {
                Either::Left(v.refs.iter())
            }
            TransitionData::AnalysisTest(_) | TransitionData::Target(_) => {
                Either::Right([].into_iter())
            }
        }
        .into_iter()
    }

    pub(crate) fn attrs(&self) -> TransitionAttrs {
        match self {
            TransitionData::BazelAttribute(_) => TransitionAttrs::BazelAll,
            TransitionData::MagicObject(v) if v.is_bazel => TransitionAttrs::BazelAll,
            TransitionData::MagicObject(v) => v
                .attrs_names
                .as_ref()
                .map(|attrs| {
                    TransitionAttrs::Listed(
                        attrs
                            .iter()
                            .map(|s| s.as_str().to_owned())
                            .collect::<Arc<[_]>>(),
                    )
                })
                .unwrap_or(TransitionAttrs::None),
            TransitionData::AnalysisTest(_) => TransitionAttrs::None,
            TransitionData::Target(v) => v
                .as_ref()
                .get_attrs_names()
                .map(|attrs| {
                    TransitionAttrs::Listed(
                        attrs
                            .into_iter()
                            .map(|s| s.to_owned())
                            .collect::<Arc<[_]>>(),
                    )
                })
                .unwrap_or(TransitionAttrs::None),
        }
    }

    pub(crate) fn is_split(&self) -> bool {
        match self {
            TransitionData::BazelAttribute(_) => true,
            TransitionData::MagicObject(v) => v.split,
            TransitionData::AnalysisTest(_) | TransitionData::Target(_) => false,
        }
    }

    pub(crate) fn is_bazel(&self) -> bool {
        match self {
            TransitionData::BazelAttribute(_) => true,
            TransitionData::MagicObject(v) => v.is_bazel,
            TransitionData::AnalysisTest(_) | TransitionData::Target(_) => false,
        }
    }

    pub(crate) fn bazel_inputs(&self) -> &[starlark::values::FrozenStringValue] {
        match self {
            TransitionData::BazelAttribute(v) => &v.inputs,
            TransitionData::MagicObject(v) if v.is_bazel => &v.inputs,
            _ => &[],
        }
    }

    pub(crate) fn bazel_canonical_build_setting_key(&self, key: &str) -> String {
        if key.starts_with("//command_line_option:") {
            return key.to_owned();
        }
        if let Some(key) = bazel_starlark_label_to_build_setting_key(key) {
            return key;
        }
        match self {
            TransitionData::MagicObject(v) if v.is_bazel && key.starts_with("//") => {
                match v.id.as_ref() {
                    TransitionId::MagicObject { path, .. } => format!("{}{}", path.cell(), key),
                    TransitionId::BazelAttribute(inner) => match inner.as_ref() {
                        TransitionId::MagicObject { path, .. } => {
                            format!("{}{}", path.cell(), key)
                        }
                        TransitionId::BazelAttribute(_)
                        | TransitionId::BazelAnalysisTest { .. }
                        | TransitionId::Target(_) => key.to_owned(),
                    },
                    TransitionId::BazelAnalysisTest { .. } | TransitionId::Target(_) => {
                        key.to_owned()
                    }
                }
            }
            TransitionData::BazelAttribute(v) if key.starts_with("//") => match v.id.as_ref() {
                TransitionId::MagicObject { path, .. } => format!("{}{}", path.cell(), key),
                TransitionId::BazelAttribute(inner) => match inner.as_ref() {
                    TransitionId::MagicObject { path, .. } => format!("{}{}", path.cell(), key),
                    TransitionId::BazelAttribute(_)
                    | TransitionId::BazelAnalysisTest { .. }
                    | TransitionId::Target(_) => key.to_owned(),
                },
                TransitionId::BazelAnalysisTest { .. } | TransitionId::Target(_) => key.to_owned(),
            },
            _ => key.to_owned(),
        }
    }

    pub(crate) fn bazel_analysis_test_settings(
        &self,
    ) -> Option<&BTreeMap<String, BazelBuildSettingValue>> {
        match self {
            TransitionData::AnalysisTest(settings) => Some(settings),
            TransitionData::MagicObject(_)
            | TransitionData::BazelAttribute(_)
            | TransitionData::Target(_) => None,
        }
    }
}

fn bazel_starlark_label_to_build_setting_key(key: &str) -> Option<String> {
    let key = key.strip_prefix("@@")?;
    let (repo, target) = key.split_once("//")?;
    let cell = if repo.is_empty() { "root" } else { repo };
    Some(format!("{cell}//{target}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bazel_starlark_label_keys_are_normalized_to_internal_build_setting_keys() {
        assert_eq!(
            bazel_starlark_label_to_build_setting_key("@@//toolchain:runtime_stage").as_deref(),
            Some("root//toolchain:runtime_stage")
        );
        assert_eq!(
            bazel_starlark_label_to_build_setting_key("@@rules_cc+//cc/compiler:msvc-cl")
                .as_deref(),
            Some("rules_cc+//cc/compiler:msvc-cl")
        );
        assert_eq!(
            bazel_starlark_label_to_build_setting_key("//toolchain:runtime_stage"),
            None
        );
    }
}

/// Fetch transition object (function plus context) by id.
#[async_trait]
pub(crate) trait FetchTransition {
    /// Fetch transition object by id.
    async fn fetch_transition(&mut self, id: &TransitionId) -> bz_error::Result<TransitionData>;
}

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum FetchTransitionError {
    #[error("Transition object not found by id {:?}", _0)]
    NotFound(TransitionId),
    #[error("Expected `{0}` to be a transition target, but it had no `TransitionInfo` provider.")]
    MissingTransitionInfo(ProvidersLabel),
    #[error("Expected `{0}` to be a Bazel Starlark transition for attribute cfg.")]
    NotBazelAttributeTransition(TransitionId),
}

#[async_trait]
impl FetchTransition for DiceComputations<'_> {
    async fn fetch_transition(&mut self, id: &TransitionId) -> bz_error::Result<TransitionData> {
        match id {
            TransitionId::MagicObject { path, name } => {
                let module = self.get_loaded_module_from_import_path(path).await?;
                let transition = module
                    .env()
                    // This is a hashmap lookup, so we are not caching the result in DICE.
                    .get_any_visibility(name)
                    .map_err(|_| bz_error::Error::from(FetchTransitionError::NotFound(id.clone())))?
                    .0;

                Ok(TransitionData::MagicObject(transition.downcast_starlark()?))
            }
            TransitionId::BazelAttribute(inner) => match inner.as_ref() {
                TransitionId::MagicObject { path, name } => {
                    let module = self.get_loaded_module_from_import_path(path).await?;
                    let transition: OwnedFrozenValueTyped<FrozenTransition> = module
                        .env()
                        .get_any_visibility(name)
                        .map_err(|_| {
                            bz_error::Error::from(FetchTransitionError::NotFound(id.clone()))
                        })?
                        .0
                        .downcast_starlark()?;
                    if !transition.is_bazel {
                        return Err(FetchTransitionError::NotBazelAttributeTransition(
                            (**inner).clone(),
                        )
                        .into());
                    }
                    Ok(TransitionData::BazelAttribute(transition))
                }
                _ => {
                    Err(FetchTransitionError::NotBazelAttributeTransition((**inner).clone()).into())
                }
            },
            TransitionId::Target(label) => {
                let transition_info = self
                    .get_configuration_analysis_result(label)
                    .await?
                    .value
                    .try_map(|c| {
                        c.as_ref()
                            .builtin_provider_value::<FrozenTransitionInfo>()
                            .ok_or_else(|| {
                                FetchTransitionError::MissingTransitionInfo(label.clone())
                            })
                    })?;
                Ok(TransitionData::Target(transition_info))
            }
            TransitionId::BazelAnalysisTest { settings } => {
                Ok(TransitionData::AnalysisTest(settings.clone()))
            }
        }
    }
}

/// Computes the attributes required by a transition.
///
/// This basically only exists so that we have a lifetime to attach to the `Arc<[String]>`, as we
/// cannot directly return the `FrozenStarlarkStr`s that are actually stored to crates that avoid
/// depending on starlark.
#[derive(
    Debug,
    Eq,
    PartialEq,
    Hash,
    Clone,
    derive_more::Display,
    allocative::Allocative,
    ref_cast::RefCast,
    Pagable
)]
#[display("{}", _0)]
#[repr(transparent)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct TransitionAttrsKey(TransitionId);

#[async_trait]
impl Key for TransitionAttrsKey {
    type Value = bz_error::Result<TransitionAttrs>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellation: &dice::CancellationContext,
    ) -> Self::Value {
        Ok(ctx.fetch_transition(&self.0).await?.attrs())
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        if let (Ok(x), Ok(y)) = (x, y) {
            x == y
        } else {
            false
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

struct TransitionGetAttrs;

#[async_trait]
impl TransitionAttrProvider for TransitionGetAttrs {
    async fn transition_attrs(
        &self,
        ctx: &mut DiceComputations<'_>,
        transition_id: &TransitionId,
    ) -> bz_error::Result<TransitionAttrs> {
        let k = TransitionAttrsKey::ref_cast(transition_id);
        ctx.compute(k).await?
    }
}

pub(crate) fn init_transition_attr_provider() {
    TRANSITION_ATTRS_PROVIDER.init(&TransitionGetAttrs);
}

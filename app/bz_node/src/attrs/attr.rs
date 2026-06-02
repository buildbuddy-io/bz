/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::fmt::Display;
use std::sync::Arc;

use allocative::Allocative;
use bz_core::package::source_path::SourcePathRef;
use bz_core::provider::label::ProvidersLabel;
use bz_core::target::label::label::TargetLabel;
use dupe::Dupe;
use pagable::Pagable;
use starlark_map::Hashed;
use starlark_map::small_set::SmallSet;

use crate::attrs::attr_type::AttrType;
use crate::attrs::attr_type::configuration_dep::ConfigurationDepKind;
use crate::attrs::coerced_attr::CoercedAttr;
use crate::attrs::display::AttrDisplayWithContextExt;
use crate::attrs::traversal::CoercedAttrTraversal;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum AttributeAllowedValuesError {
    #[error("value `{value}` is not one of the allowed values: {}", .allowed.join(", "))]
    InvalidValue { value: String, allowed: Vec<String> },
}

#[derive(Clone, Debug, Hash, Eq, PartialEq, Pagable, Allocative)]
enum AttributeDefault {
    No,
    Yes(Arc<CoercedAttr>),
    YesWithAllowed {
        default: Arc<CoercedAttr>,
        allowed_deps: Hashed<SmallSet<TargetLabel>>,
    },
    // N.B. DefaultOnly attributes are not checked for within_view, so we don't have to track allowed_deps for them
    DefaultOnly(Arc<CoercedAttr>),
}

/// Starlark compatible container for results from e.g. `attrs.string()`
#[derive(Clone, Debug, Eq, PartialEq, Hash, Pagable, Allocative)]
pub struct Attribute {
    /// The default value. If None, the value is not optional and must be provided by the user
    default: AttributeDefault,
    /// Documentation for what the attribute actually means
    doc: String,
    /// The coercer to take this parameter's value from Starlark value -> an
    /// internal representation
    coercer: AttrType,
    /// Bazel `values` restrictions validate explicit target values without changing attr type.
    allowed_values: Option<AttributeAllowedValues>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Pagable, Allocative)]
pub struct AttributeAllowedValues {
    values: Box<[CoercedAttr]>,
}

impl AttributeAllowedValues {
    pub fn new(values: Vec<CoercedAttr>) -> Option<Self> {
        if values.is_empty() {
            None
        } else {
            Some(Self {
                values: values.into_boxed_slice(),
            })
        }
    }

    pub fn validate(&self, value: &CoercedAttr) -> bz_error::Result<()> {
        match value {
            CoercedAttr::Selector(selector) => {
                for (_, value) in selector.all_entries() {
                    self.validate(value)?;
                }
                Ok(())
            }
            CoercedAttr::Concat(concat) => {
                for value in concat.iter() {
                    self.validate(value)?;
                }
                Ok(())
            }
            CoercedAttr::SelectFail(_) | CoercedAttr::SelectIncompatible(_) => Ok(()),
            CoercedAttr::OneOf(value, _) => self.validate(value),
            value => {
                if self.values.iter().any(|allowed| allowed == value) {
                    Ok(())
                } else {
                    Err(AttributeAllowedValuesError::InvalidValue {
                        value: value.as_display_no_ctx().to_string(),
                        allowed: self
                            .values
                            .iter()
                            .map(|value| value.as_display_no_ctx().to_string())
                            .collect(),
                    }
                    .into())
                }
            }
        }
    }
}

impl Attribute {
    pub fn new_const(default: Option<Arc<CoercedAttr>>, doc: &str, coercer: AttrType) -> Self {
        Self::new(default, doc, coercer).expect("Attribute::new_const failed")
    }

    pub fn new(
        default: Option<Arc<CoercedAttr>>,
        doc: &str,
        coercer: AttrType,
    ) -> bz_error::Result<Self> {
        Self::new_with_allowed_values(default, doc, coercer, None)
    }

    pub fn new_with_allowed_values(
        default: Option<Arc<CoercedAttr>>,
        doc: &str,
        coercer: AttrType,
        allowed_values: Option<AttributeAllowedValues>,
    ) -> bz_error::Result<Self> {
        Ok(Attribute {
            default: match default {
                Some(default) => {
                    let allowed_deps = collect_default_deps(&default, &coercer)?;
                    if allowed_deps.is_empty() {
                        AttributeDefault::Yes(default)
                    } else {
                        AttributeDefault::YesWithAllowed {
                            default,
                            allowed_deps: allowed_deps.hashed(),
                        }
                    }
                }
                None => AttributeDefault::No,
            },
            doc: doc.to_owned(),
            coercer,
            allowed_values,
        })
    }

    pub fn new_default_only(default: Arc<CoercedAttr>, doc: &str, coercer: AttrType) -> Self {
        Attribute {
            default: AttributeDefault::DefaultOnly(default),
            doc: doc.to_owned(),
            coercer,
            allowed_values: None,
        }
    }

    pub fn coercer(&self) -> &AttrType {
        &self.coercer
    }

    pub fn is_default_only(&self) -> bool {
        matches!(self.default, AttributeDefault::DefaultOnly(_))
    }

    pub fn default(&self) -> Option<&Arc<CoercedAttr>> {
        match &self.default {
            AttributeDefault::Yes(x) | AttributeDefault::YesWithAllowed { default: x, .. } => {
                Some(x)
            }
            AttributeDefault::DefaultOnly(x) => Some(x),
            AttributeDefault::No => None,
        }
    }

    pub fn default_allowed_deps(&self) -> Option<&SmallSet<TargetLabel>> {
        match &self.default {
            AttributeDefault::YesWithAllowed { allowed_deps, .. } => Some(&**allowed_deps),
            AttributeDefault::Yes(_) | AttributeDefault::DefaultOnly(_) | AttributeDefault::No => {
                None
            }
        }
    }

    pub fn doc(&self) -> &str {
        &self.doc
    }

    pub fn validate_allowed_values(&self, value: &CoercedAttr) -> bz_error::Result<()> {
        match &self.allowed_values {
            Some(allowed_values) => allowed_values.validate(value),
            None => Ok(()),
        }
    }
}

impl Display for Attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.coercer.fmt_with_default(
            f,
            self.default()
                .map(|x| x.as_display_no_ctx().to_string())
                .as_deref(),
        )
    }
}

/// Attribute which may be either a custom value supplied by the user, or missing/None to indicate use the default.
#[derive(Eq, PartialEq)]
pub enum CoercedValue {
    Custom(CoercedAttr),
    Default,
}

fn collect_default_deps(
    default: &Arc<CoercedAttr>,
    attr_type: &AttrType,
) -> bz_error::Result<SmallSet<TargetLabel>> {
    struct CollectDefaultsTraversal<'a> {
        deps: &'a mut SmallSet<TargetLabel>,
    }

    // N.B. the traversal here needs to match the `dep` cases that check_within_view checks
    impl<'a> CoercedAttrTraversal<'a> for CollectDefaultsTraversal<'a> {
        fn dep(&mut self, dep: &ProvidersLabel) -> bz_error::Result<()> {
            self.deps.insert(dep.target().dupe());
            Ok(())
        }

        fn configuration_dep(
            &mut self,
            dep: &ProvidersLabel,
            t: ConfigurationDepKind,
        ) -> bz_error::Result<()> {
            match t {
                // Skip some configuration deps
                ConfigurationDepKind::CompatibilityAttribute
                | ConfigurationDepKind::DefaultTargetPlatform
                | ConfigurationDepKind::SelectKey => (),
                ConfigurationDepKind::ConfiguredDepPlatform | ConfigurationDepKind::Transition => {
                    self.deps.insert(dep.target().dupe());
                }
            }
            Ok(())
        }

        fn input(&mut self, _input: SourcePathRef) -> bz_error::Result<()> {
            Ok(())
        }

        fn inputs_require_package(&self) -> bool {
            false
        }
    }

    let mut default_deps = SmallSet::new();
    default.traverse(
        attr_type,
        None,
        &mut CollectDefaultsTraversal {
            deps: &mut default_deps,
        },
    )?;
    Ok(default_deps)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bz_core::package::package_relative_path::PackageRelativePathBuf;

    use crate::attrs::attr::Attribute;
    use crate::attrs::attr_type::AttrType;
    use crate::attrs::coerced_attr::CoercedAttr;
    use crate::attrs::coerced_path::CoercedPath;

    #[test]
    fn source_file_defaults_do_not_need_package_to_collect_default_deps() -> bz_error::Result<()>
    {
        let path = PackageRelativePathBuf::unchecked_new("LICENSE".to_owned());
        let default = Arc::new(CoercedAttr::SourceFile(CoercedPath::File(
            path.as_path().to_arc(),
        )));

        let attr = Attribute::new(Some(default), "", AttrType::source(false))?;

        assert!(attr.default_allowed_deps().is_none());
        Ok(())
    }
}

use allocative::Allocative;
use bz_core::provider::label::ConfiguredProvidersLabel;
use bz_core::provider::label::ProvidersLabel;
use bz_error::internal_error;
use pagable::Pagable;

use crate::attrs::attr_type::AttrType;
use crate::attrs::attr_type::AttrTypeInner;
use crate::attrs::attr_type::dep::DepAttr;
use crate::attrs::attr_type::dep::DepAttrTransition;
use crate::attrs::attr_type::split_transition_dep::ConfiguredSplitTransitionDep;
use crate::attrs::configured_attr::ConfiguredAttr;
use crate::attrs::configured_traversal::ConfiguredAttrTraversal;
use crate::attrs::configuration_context::AttrConfigurationContext;
use crate::attrs::display::AttrDisplayWithContextExt;
use crate::attrs::traversal::CoercedAttrTraversal;

#[derive(Debug, Clone, Hash, Pagable, Eq, PartialEq, Allocative)]
pub enum BazelAllowedFileTypes {
    None,
    Any,
    Extensions(Box<[String]>),
}

impl BazelAllowedFileTypes {
    pub fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Any, _) | (_, Self::Any) => Self::Any,
            (Self::None, other) => other,
            (this, Self::None) => this,
            (Self::Extensions(left), Self::Extensions(right)) => {
                let mut extensions = Vec::with_capacity(left.len() + right.len());
                extensions.extend(left);
                extensions.extend(right);
                extensions.sort();
                extensions.dedup();
                if extensions.is_empty() {
                    Self::None
                } else {
                    Self::Extensions(extensions.into_boxed_slice())
                }
            }
        }
    }

    pub fn allows_files(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub fn allows_target_name(&self, target_name: &str) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Extensions(extensions) => {
                extensions.iter().any(|extension| target_name.ends_with(extension))
            }
        }
    }
}

#[derive(Debug, Hash, Pagable, Eq, PartialEq, Allocative)]
pub struct BazelLabelAttrType {
    pub dep: AttrType,
    pub source: AttrType,
    pub allowed_files: BazelAllowedFileTypes,
}

impl BazelLabelAttrType {
    pub fn new(dep: AttrType, source: AttrType, allowed_files: BazelAllowedFileTypes) -> Self {
        Self {
            dep,
            source,
            allowed_files,
        }
    }

    pub fn configure(
        &self,
        label: &ProvidersLabel,
        ctx: &dyn AttrConfigurationContext,
    ) -> bz_error::Result<ConfiguredAttr> {
        let dep = match &self.dep.0.inner {
            AttrTypeInner::Dep(t) => match t.configure(label, ctx)? {
                ConfiguredAttr::Dep(dep) => ConfiguredBazelLabelDep::Dep(dep),
                attr => {
                    return Err(internal_error!(
                        "Expected configured dep for Bazel label attr, got `{}`",
                        attr.as_display_no_ctx()
                    ));
                }
            },
            AttrTypeInner::SplitTransitionDep(t) => match t.configure(label, ctx)? {
                ConfiguredAttr::SplitTransitionDep(dep) => {
                    ConfiguredBazelLabelDep::SplitTransition(dep)
                }
                attr => {
                    return Err(internal_error!(
                        "Expected configured split-transition dep for Bazel label attr, got `{}`",
                        attr.as_display_no_ctx()
                    ));
                }
            },
            other => {
                return Err(internal_error!(
                    "Expected dependency type inside Bazel label attr, got `{:?}`",
                    other
                ));
            }
        };
        Ok(ConfiguredAttr::BazelLabel(Box::new(ConfiguredBazelLabel {
            dep,
            allowed_files: self.allowed_files.clone(),
        })))
    }

    pub fn traverse<'a>(
        &self,
        label: &'a ProvidersLabel,
        traversal: &mut dyn CoercedAttrTraversal<'a>,
    ) -> bz_error::Result<()> {
        match &self.dep.0.inner {
            AttrTypeInner::Dep(t) => DepAttr::<ProvidersLabel>::traverse(label, t, traversal),
            AttrTypeInner::SplitTransitionDep(t) => {
                traversal.split_transition_dep(label, &t.transition)
            }
            other => Err(internal_error!(
                "Expected dependency type inside Bazel label attr, got `{:?}`",
                other
            )
            .into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
pub struct ConfiguredBazelLabel {
    pub dep: ConfiguredBazelLabelDep,
    pub allowed_files: BazelAllowedFileTypes,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Allocative, Pagable)]
pub enum ConfiguredBazelLabelDep {
    Dep(Box<DepAttr<ConfiguredProvidersLabel>>),
    SplitTransition(Box<ConfiguredSplitTransitionDep>),
}

impl ConfiguredBazelLabel {
    pub fn first_label(&self) -> Option<&ConfiguredProvidersLabel> {
        match &self.dep {
            ConfiguredBazelLabelDep::Dep(dep) => Some(&dep.label),
            ConfiguredBazelLabelDep::SplitTransition(dep) => dep.deps.values().next(),
        }
    }

    pub fn traverse(&self, traversal: &mut dyn ConfiguredAttrTraversal) -> bz_error::Result<()> {
        match &self.dep {
            ConfiguredBazelLabelDep::Dep(dep) => dep.traverse(traversal),
            ConfiguredBazelLabelDep::SplitTransition(dep) => {
                for target in dep.deps.values() {
                    traversal.dep(target)?;
                }
                Ok(())
            }
        }
    }

    pub fn is_exec_dep(dep: &DepAttr<ConfiguredProvidersLabel>) -> bool {
        matches!(
            &dep.attr_type.transition,
            DepAttrTransition::Exec | DepAttrTransition::Toolchain
        )
    }
}

impl std::fmt::Display for ConfiguredBazelLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.dep {
            ConfiguredBazelLabelDep::Dep(dep) => std::fmt::Display::fmt(dep, f),
            ConfiguredBazelLabelDep::SplitTransition(dep) => std::fmt::Display::fmt(dep, f),
        }
    }
}

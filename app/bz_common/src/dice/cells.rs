/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Core dice computations relating to cells

use allocative::Allocative;
use async_trait::async_trait;
use bz_core::cells::CellAliasResolver;
use bz_core::cells::CellResolver;
use bz_core::cells::external::BZLMOD_EXTERNAL_CELL_KIND;
use bz_core::cells::external::BZLMOD_GENERATED_EXTERNAL_CELL_KIND;
use bz_core::cells::external::ExternalCellOrigin;
use bz_core::cells::external::external_cell_origin_for_cell;
use bz_core::cells::external::external_cell_source_path;
use bz_core::cells::external::is_bzlmod_cell_name;
use bz_core::cells::name::CellName;
use bz_core::fs::project_rel_path::ProjectRelativePath;
use derive_more::Display;
use dice::CancellationContext;
use dice::DiceComputations;
use dice::DiceTransactionUpdater;
use dice::InjectedKey;
use dice::InvalidationSourcePriority;
use dice::Key;
use dice::OkPagableValueSerialize;
use dice::PagableValueSerialize;
use dice::ValueSerialize;
use dupe::Dupe;
use pagable::Pagable;
use pagable::pagable_typetag;

use crate::bazel::bzlmod::bzlmod_resolution_enabled_on_dice;
use crate::bazel::bzlmod::get_bazel_module_resolution_on_dice;
use crate::external_cells::EXTERNAL_CELLS_IMPL;
use crate::legacy_configs::cells::BuckConfigBasedCells;
use crate::legacy_configs::configs::BazelCompatBazelrcOptions;
use crate::legacy_configs::configs::LegacyBuckConfig;
use crate::legacy_configs::dice::HasLegacyConfigs;

#[async_trait]
pub trait HasCellResolver {
    async fn get_cell_resolver(&mut self) -> bz_error::Result<CellResolver>;

    async fn is_cell_resolver_key_set(&mut self) -> bz_error::Result<bool>;

    async fn get_cell_alias_resolver(
        &mut self,
        cell: CellName,
    ) -> bz_error::Result<CellAliasResolver>;

    async fn get_cell_alias_resolver_for_dir(
        &mut self,
        dir: &ProjectRelativePath,
    ) -> bz_error::Result<CellAliasResolver>;
}

pub trait SetCellResolver {
    fn set_cell_resolver(&mut self, cell_resolver: CellResolver) -> bz_error::Result<()>;

    fn set_none_cell_resolver(&mut self) -> bz_error::Result<()>;
}

pub trait SetExternalCellOrigins {
    fn set_external_cell_origins_from_cell_resolver(
        &mut self,
        cell_resolver: &CellResolver,
    ) -> bz_error::Result<()>;

    fn set_changed_external_cell_origins(
        &mut self,
        previous: &CellResolver,
        current: &CellResolver,
    ) -> bz_error::Result<()>;
}

#[async_trait]
pub trait HasExternalCellOrigins {
    async fn get_external_cell_origin(
        &mut self,
        cell: CellName,
    ) -> bz_error::Result<Option<ExternalCellOrigin>>;
}

#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("{:?}", self)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct CellResolverKey;

impl InjectedKey for CellResolverKey {
    type Value = Option<CellResolver>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Some(x), Some(y)) => cell_resolver_graph_shape_equal(x, y),
            (None, None) => true,
            _ => false,
        }
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::Ignored
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        PagableValueSerialize::<Self::Value>::new()
    }
}

#[derive(Clone, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ExternalCellOriginKey({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct ExternalCellOriginKey(CellName);

impl InjectedKey for ExternalCellOriginKey {
    type Value = Option<ExternalCellOrigin>;

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn invalidation_source_priority() -> InvalidationSourcePriority {
        InvalidationSourcePriority::Ignored
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        PagableValueSerialize::<Self::Value>::new()
    }
}

pub fn cell_resolver_graph_shape_equal(x: &CellResolver, y: &CellResolver) -> bool {
    if x.root_cell() != y.root_cell()
        || x.root_cell_cell_alias_resolver() != y.root_cell_cell_alias_resolver()
    {
        return false;
    }

    let mut count = 0;
    for (cell, x_instance) in x.cells() {
        count += 1;
        let Ok(y_instance) = y.get(cell) else {
            return false;
        };
        if x_instance.path() != y_instance.path()
            || x_instance.nested_cells() != y_instance.nested_cells()
            || !external_cell_origin_shape_equal(x_instance.external(), y_instance.external())
        {
            return false;
        }
    }
    count == y.cells().count()
}

fn external_cell_origin_shape_equal(
    x: Option<&ExternalCellOrigin>,
    y: Option<&ExternalCellOrigin>,
) -> bool {
    match (x, y) {
        (None, None) => true,
        (Some(ExternalCellOrigin::Bundled(x)), Some(ExternalCellOrigin::Bundled(y))) => x == y,
        (Some(ExternalCellOrigin::Git(_)), Some(ExternalCellOrigin::Git(_))) => true,
        (Some(ExternalCellOrigin::Bzlmod(x)), Some(ExternalCellOrigin::Bzlmod(y))) => {
            x.canonical_repo_name == y.canonical_repo_name
        }
        (
            Some(ExternalCellOrigin::BzlmodGenerated(x)),
            Some(ExternalCellOrigin::BzlmodGenerated(y)),
        ) => x.canonical_repo_name == y.canonical_repo_name,
        _ => false,
    }
}

fn is_declared_bzlmod_external_cell(resolver: &CellResolver, cell: CellName) -> bool {
    if !resolver.contains_declared(cell) {
        return false;
    }
    let Ok(instance) = resolver.get(cell) else {
        return false;
    };
    let cell_path = instance.path().as_project_relative_path().as_str();
    [
        BZLMOD_EXTERNAL_CELL_KIND,
        BZLMOD_GENERATED_EXTERNAL_CELL_KIND,
    ]
    .into_iter()
    .any(|kind| {
        let prefix = external_cell_source_path(kind, "");
        let Some(canonical_repo_name) = cell_path.strip_prefix(&prefix) else {
            return false;
        };
        !canonical_repo_name.is_empty() && !canonical_repo_name.contains('/')
    })
}

#[async_trait]
impl HasCellResolver for DiceComputations<'_> {
    async fn get_cell_resolver(&mut self) -> bz_error::Result<CellResolver> {
        self.compute(&CellResolverKey).await?.ok_or_else(|| {
            panic!("Tried to retrieve CellResolverKey from the graph, but key has None value")
        })
    }

    async fn is_cell_resolver_key_set(&mut self) -> bz_error::Result<bool> {
        Ok(self.compute(&CellResolverKey).await?.is_some())
    }

    async fn get_cell_alias_resolver(
        &mut self,
        cell: CellName,
    ) -> bz_error::Result<CellAliasResolver> {
        Ok(self.compute(&CellAliasResolverKey(cell)).await??)
    }

    async fn get_cell_alias_resolver_for_dir(
        &mut self,
        dir: &ProjectRelativePath,
    ) -> bz_error::Result<CellAliasResolver> {
        let cell = self.get_cell_resolver().await?.find(dir);
        self.get_cell_alias_resolver(cell).await
    }
}

#[async_trait]
impl HasExternalCellOrigins for DiceComputations<'_> {
    async fn get_external_cell_origin(
        &mut self,
        cell: CellName,
    ) -> bz_error::Result<Option<ExternalCellOrigin>> {
        let resolver = self.compute(&CellResolverKey).await?;
        if is_bzlmod_cell_name(cell.as_str()) {
            let cell_in_resolver = resolver
                .as_ref()
                .is_some_and(|resolver| resolver.contains_declared(cell));
            if !cell_in_resolver {
                if external_cell_origin_for_cell(cell.as_str()).is_none() {
                    let _aliases = get_bazel_module_resolution_on_dice(self).await?;
                }
                return Ok(external_cell_origin_for_cell(cell.as_str()));
            }
        }
        let origin = self.compute(&ExternalCellOriginKey(cell)).await?;
        if origin.is_some() {
            return Ok(origin);
        }
        let declared_bzlmod_external_cell = resolver
            .as_ref()
            .is_some_and(|resolver| is_declared_bzlmod_external_cell(resolver, cell));
        if is_bzlmod_cell_name(cell.as_str()) || declared_bzlmod_external_cell {
            if external_cell_origin_for_cell(cell.as_str()).is_none() {
                let _aliases = get_bazel_module_resolution_on_dice(self).await?;
            }
            if let Some(origin) = external_cell_origin_for_cell(cell.as_str()) {
                return Ok(Some(origin));
            }
        }
        Ok(None)
    }
}

/// Only used for cell alias resolvers parsed within dice, currently those for external cells
#[derive(Clone, Dupe, Display, Debug, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("REPOSITORY_MAPPING({})", _0)]
#[pagable_typetag(dice::DiceKeyDyn)]
struct CellAliasResolverKey(CellName);

#[async_trait]
impl Key for CellAliasResolverKey {
    type Value = bz_error::Result<CellAliasResolver>;

    async fn compute(
        &self,
        ctx: &mut DiceComputations,
        _cancellations: &CancellationContext,
    ) -> Self::Value {
        let resolver = ctx.get_cell_resolver().await?;
        let root_aliases = resolver.root_cell_cell_alias_resolver();
        let declared_bzlmod_external_cell = is_declared_bzlmod_external_cell(&resolver, self.0);
        let bzlmod_module_aliases = if (self.0 == resolver.root_cell()
            || self.0.as_str() == "bazel_tools"
            || is_bzlmod_cell_name(self.0.as_str())
            || declared_bzlmod_external_cell)
            && bzlmod_resolution_enabled_on_dice(ctx).await?
        {
            Some(get_bazel_module_resolution_on_dice(ctx).await?)
        } else {
            None
        };
        let cell = resolver.get(self.0).ok();
        let cell_exists = cell.is_some();
        let external_origin = match cell.and_then(|cell| cell.external().map(Dupe::dupe)) {
            Some(origin) => Some(origin),
            None if !cell_exists => ctx.get_external_cell_origin(self.0).await?,
            None => None,
        };
        if let Some(ExternalCellOrigin::BzlmodGenerated(_)) = &external_origin {
            EXTERNAL_CELLS_IMPL
                .get()?
                .ensure_cell_alias_resolver_ready(
                    ctx,
                    self.0,
                    external_origin.dupe().expect("origin checked above"),
                )
                .await?;
        }
        if !cell_exists
            && matches!(
                &external_origin,
                Some(ExternalCellOrigin::BzlmodGenerated(_))
            )
        {
            return BuckConfigBasedCells::get_bazel_cell_alias_resolver_from_config(
                self.0,
                &resolver,
                &crate::legacy_configs::configs::LegacyBuckConfig::empty(),
            );
        }
        if (self.0.as_str() == "bazel_tools" || is_bzlmod_cell_name(self.0.as_str()))
            && let Some(module_aliases) = &bzlmod_module_aliases
        {
            let current_cell_aliases =
                module_aliases.aliases_for_cell(self.0.as_str(), resolver.root_cell().as_str());
            let config = LegacyBuckConfig::empty().with_bazel_compat_cell_defaults(
                &current_cell_aliases,
                &[],
                &BazelCompatBazelrcOptions::default(),
            );
            if self.0.as_str() == "bazel_tools" {
                resolver.get(self.0).map_err(|_| {
                    bz_error::bz_error!(bz_error::ErrorTag::Input, "Unknown cell `{}`", self.0)
                })?;
            } else if !cell_exists && external_origin.is_none() {
                return Err(bz_error::bz_error!(
                    bz_error::ErrorTag::Input,
                    "Unknown cell `{}`",
                    self.0
                ));
            }
            return BuckConfigBasedCells::get_bazel_cell_alias_resolver_from_config(
                self.0, &resolver, &config,
            );
        }
        let config = ctx.get_legacy_config_for_cell(self.0).await?;
        let config = if let Some(module_aliases) = &bzlmod_module_aliases {
            let current_cell_aliases =
                module_aliases.aliases_for_cell(self.0.as_str(), resolver.root_cell().as_str());
            config.with_bazel_compat_cell_defaults(
                &current_cell_aliases,
                &[],
                &BazelCompatBazelrcOptions::default(),
            )
        } else {
            config
        };
        resolver.get(self.0).map_err(|_| {
            bz_error::bz_error!(bz_error::ErrorTag::Input, "Unknown cell `{}`", self.0)
        })?;
        if bzlmod_module_aliases.is_some()
            || self.0.as_str() == "bazel_tools"
            || matches!(
                &external_origin,
                Some(ExternalCellOrigin::Bzlmod(_)) | Some(ExternalCellOrigin::BzlmodGenerated(_))
            )
        {
            return BuckConfigBasedCells::get_bazel_cell_alias_resolver_from_config(
                self.0, &resolver, &config,
            );
        }
        // Cell alias resolvers that are parsed within dice differ from those outside of dice in
        // that they cannot create new cells, and so respect only their `cell_aliases` section, not
        // their `cells` section. This is the expected behavior for external cells, moving other
        // cell resolver parsing into dice would require this code to be adjusted.
        CellAliasResolver::new_for_non_root_cell(
            self.0,
            root_aliases,
            BuckConfigBasedCells::get_cell_aliases_from_config(&config)?,
        )
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (Ok(x), Ok(y)) => x == y,
            (_, _) => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        OkPagableValueSerialize::<Self::Value>::new()
    }
}

impl SetCellResolver for DiceTransactionUpdater {
    fn set_cell_resolver(&mut self, cell_resolver: CellResolver) -> bz_error::Result<()> {
        self.set_external_cell_origins_from_cell_resolver(&cell_resolver)?;
        Ok(self.changed_to(vec![(CellResolverKey, Some(cell_resolver))])?)
    }

    fn set_none_cell_resolver(&mut self) -> bz_error::Result<()> {
        Ok(self.changed_to(vec![(CellResolverKey, None)])?)
    }
}

impl SetExternalCellOrigins for DiceTransactionUpdater {
    fn set_external_cell_origins_from_cell_resolver(
        &mut self,
        cell_resolver: &CellResolver,
    ) -> bz_error::Result<()> {
        let origins = cell_resolver
            .cells()
            .map(|(cell, instance)| {
                (
                    ExternalCellOriginKey(cell),
                    instance.external().map(|origin| origin.dupe()),
                )
            })
            .collect::<Vec<_>>();
        Ok(self.changed_to(origins)?)
    }

    fn set_changed_external_cell_origins(
        &mut self,
        previous: &CellResolver,
        current: &CellResolver,
    ) -> bz_error::Result<()> {
        let mut changed = Vec::new();
        for (cell, current_instance) in current.cells() {
            let previous_origin = previous
                .get(cell)
                .ok()
                .and_then(|instance| instance.external());
            if previous_origin != current_instance.external() {
                changed.push((
                    ExternalCellOriginKey(cell),
                    current_instance.external().map(|origin| origin.dupe()),
                ));
            }
        }
        Ok(self.changed_to(changed)?)
    }
}

#[cfg(test)]
mod tests {
    use bz_core::cells::cell_root_path::CellRootPathBuf;
    use bz_core::cells::instance::CellInstance;
    use bz_core::cells::nested::NestedCells;
    use bz_hash::StdBuckHashMap;

    use super::*;

    fn resolver_with_extra_cell(
        cell_name: &str,
        cell_path: &str,
    ) -> bz_error::Result<CellResolver> {
        let root = CellName::testing_new("root");
        let extra = CellName::testing_new(cell_name);
        let root_aliases = CellAliasResolver::new(root, StdBuckHashMap::default())?;
        CellResolver::new(
            vec![
                CellInstance::new(
                    root,
                    CellRootPathBuf::testing_new(""),
                    None,
                    NestedCells::empty(),
                )?,
                CellInstance::new(
                    extra,
                    CellRootPathBuf::testing_new(cell_path),
                    None,
                    NestedCells::empty(),
                )?,
            ],
            root_aliases,
        )
    }

    #[test]
    fn detects_declared_bzlmod_external_cell() -> bz_error::Result<()> {
        let resolver =
            resolver_with_extra_cell("platforms", "buck-out/v2/external_cells/bzlmod/platforms")?;

        assert!(is_declared_bzlmod_external_cell(
            &resolver,
            CellName::testing_new("platforms")
        ));

        Ok(())
    }

    #[test]
    fn ignores_regular_declared_cell() -> bz_error::Result<()> {
        let resolver = resolver_with_extra_cell("platforms", "third_party/platforms")?;

        assert!(!is_declared_bzlmod_external_cell(
            &resolver,
            CellName::testing_new("platforms")
        ));

        Ok(())
    }

    #[test]
    fn ignores_nested_path_under_bzlmod_external_root() -> bz_error::Result<()> {
        let resolver = resolver_with_extra_cell(
            "platforms_host",
            "buck-out/v2/external_cells/bzlmod/platforms/host",
        )?;

        assert!(!is_declared_bzlmod_external_cell(
            &resolver,
            CellName::testing_new("platforms_host")
        ));

        Ok(())
    }
}

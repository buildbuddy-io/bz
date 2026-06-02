use buck2_core::cells::CellAliasResolver;
use buck2_core::cells::CellResolver;
use buck2_core::cells::name::CellName;
use buck2_core::package::PackageLabel;
use starlark::any::ProvidesStaticType;

#[derive(Debug, ProvidesStaticType)]
pub struct StarlarkLabelResolutionContext {
    pub cell_name: CellName,
    pub cell_resolver: CellResolver,
    pub cell_alias_resolver: CellAliasResolver,
    pub package: Option<PackageLabel>,
}

impl StarlarkLabelResolutionContext {
    pub fn new(
        cell_name: CellName,
        cell_resolver: CellResolver,
        cell_alias_resolver: CellAliasResolver,
        package: Option<PackageLabel>,
    ) -> Self {
        Self {
            cell_name,
            cell_resolver,
            cell_alias_resolver,
            package,
        }
    }
}

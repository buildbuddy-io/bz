use bz_node::attrs::attr_type::bazel::label::BazelLabelAttrType;
use bz_node::attrs::coerced_attr::CoercedAttr;
use bz_node::attrs::coercion_context::AttrCoercionContext;
use bz_node::attrs::configurable::AttrIsConfigurable;
use starlark::typing::Ty;
use starlark::values::Value;

use crate::attrs::coerce::AttrTypeCoerce;
use crate::attrs::coerce::attr_type::AttrTypeExt;
use crate::attrs::coerce::attr_type::ty_maybe_select::TyMaybeSelect;

fn looks_like_label(value: &str) -> bool {
    value.contains(':') || value.starts_with('@') || value.starts_with("//")
}

impl AttrTypeCoerce for BazelLabelAttrType {
    fn coerce_item(
        &self,
        configurable: AttrIsConfigurable,
        ctx: &dyn AttrCoercionContext,
        value: Value,
    ) -> bz_error::Result<CoercedAttr> {
        if let Some(value_str) = value.unpack_str() {
            if ctx.is_bazel_compat_cell() {
                // This attr type is only built when `allow_files` is set, so source
                // files are permitted alongside rule deps. Bazel exempts source
                // files matching `allow_files` (e.g. `.lds` linker scripts listed in
                // a cc rule's `deps`) from the attribute's provider requirement. A
                // *bare* relative reference (no `:`, `@`, or `//`) may name an actual
                // source file; try source coercion first for those (it only succeeds
                // for a file present in the package listing) and fall back to a
                // dependency. Explicit label forms (`:foo`, `//pkg:foo`, `@repo//...`)
                // always resolve as dependencies, matching Bazel — otherwise a label
                // pointing at a rule target's output (e.g. a TreeArtifact) could be
                // mis-coerced as a source.
                if !looks_like_label(value_str) {
                    if let Ok(coerced) = self.source.coerce_item(configurable, ctx, value) {
                        return Ok(coerced);
                    }
                }
                return self
                    .dep
                    .coerce_item(configurable, ctx, value)
                    .map_err(|dep_error| {
                        bz_error::bz_error!(
                            bz_error::ErrorTag::Input,
                            "could not coerce Bazel label as dependency ({:#})",
                            dep_error
                        )
                    });
            }
            if !looks_like_label(value_str) {
                if let Ok(value) = self.source.coerce_item(configurable, ctx, value) {
                    return Ok(value);
                }
            }
        }

        match self.dep.coerce_item(configurable, ctx, value) {
            Ok(value) => Ok(value),
            Err(dep_error) => {
                self.source
                    .coerce_item(configurable, ctx, value)
                    .map_err(|source_error| {
                        bz_error::bz_error!(
                            bz_error::ErrorTag::Input,
                            "could not coerce Bazel label as dependency ({:#}) or source ({:#})",
                            dep_error,
                            source_error
                        )
                    })
            }
        }
    }

    fn starlark_type(&self) -> TyMaybeSelect {
        TyMaybeSelect::Basic(Ty::string())
    }
}

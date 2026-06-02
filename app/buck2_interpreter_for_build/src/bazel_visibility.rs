use std::borrow::Cow;

use buck2_core::package::PackageLabel;
use buck2_node::attrs::coercion_context::AttrCoercionContext;
use buck2_node::visibility::VisibilityPattern;
use buck2_node::visibility::VisibilityWithinViewBuilder;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NormalizedVisibilityPattern<'a> {
    Public,
    Private,
    Pattern(Cow<'a, str>),
}

pub(crate) fn normalize_visibility_pattern<'a>(
    pattern: &'a str,
    enclosing_package: Option<&PackageLabel>,
) -> NormalizedVisibilityPattern<'a> {
    match pattern {
        VisibilityPattern::PUBLIC | "//visibility:public" => NormalizedVisibilityPattern::Public,
        "//visibility:private" => NormalizedVisibilityPattern::Private,
        _ => {
            if pattern == ":__pkg__"
                && let Some(enclosing_package) = enclosing_package
            {
                NormalizedVisibilityPattern::Pattern(Cow::Owned(format!("{enclosing_package}:")))
            } else if pattern == ":__subpackages__"
                && let Some(enclosing_package) = enclosing_package
            {
                let package = enclosing_package.to_string();
                let pattern = if package.ends_with("//") {
                    format!("{package}...")
                } else {
                    format!("{package}/...")
                };
                NormalizedVisibilityPattern::Pattern(Cow::Owned(pattern))
            } else if let Some(package) = pattern.strip_suffix(":__pkg__") {
                NormalizedVisibilityPattern::Pattern(Cow::Owned(format!("{package}:")))
            } else if let Some(package) = pattern.strip_suffix(":__subpackages__") {
                let pattern = if package.is_empty() {
                    "...".to_owned()
                } else if package.ends_with("//") {
                    format!("{package}...")
                } else {
                    format!("{package}/...")
                };
                NormalizedVisibilityPattern::Pattern(Cow::Owned(pattern))
            } else {
                NormalizedVisibilityPattern::Pattern(Cow::Borrowed(pattern))
            }
        }
    }
}

pub(crate) fn add_visibility_pattern(
    builder: &mut VisibilityWithinViewBuilder,
    ctx: &dyn AttrCoercionContext,
    pattern: &str,
) -> buck2_error::Result<()> {
    match normalize_visibility_pattern(pattern, ctx.enclosing_package().as_ref()) {
        NormalizedVisibilityPattern::Public => builder.add_public(),
        NormalizedVisibilityPattern::Private => {}
        NormalizedVisibilityPattern::Pattern(pattern) => {
            if let Some(pattern) = ctx.coerce_visibility_pattern(&pattern)? {
                builder.add(VisibilityPattern(pattern));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use buck2_core::package::PackageLabel;

    use super::NormalizedVisibilityPattern;
    use super::normalize_visibility_pattern;

    #[test]
    fn normalize_bazel_relative_visibility_package_markers() {
        let package = PackageLabel::testing_new("root", "server/cmd/buildbuddy");

        assert_eq!(
            NormalizedVisibilityPattern::Pattern(Cow::Owned(
                "root//server/cmd/buildbuddy:".to_owned()
            )),
            normalize_visibility_pattern(":__pkg__", Some(&package))
        );
        assert_eq!(
            NormalizedVisibilityPattern::Pattern(Cow::Owned(
                "root//server/cmd/buildbuddy/...".to_owned()
            )),
            normalize_visibility_pattern(":__subpackages__", Some(&package))
        );
    }

    #[test]
    fn normalize_bazel_relative_visibility_root_package() {
        let package = PackageLabel::testing_new("root", "");

        assert_eq!(
            NormalizedVisibilityPattern::Pattern(Cow::Owned("root//...".to_owned())),
            normalize_visibility_pattern(":__subpackages__", Some(&package))
        );
    }

    #[test]
    fn preserve_relative_package_group_labels() {
        assert_eq!(
            NormalizedVisibilityPattern::Pattern(Cow::Borrowed(":vis")),
            normalize_visibility_pattern(":vis", None)
        );
    }
}

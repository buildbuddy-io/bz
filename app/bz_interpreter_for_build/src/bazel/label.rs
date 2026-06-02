use bz_core::cells::external::bzlmod_cell_name;
use bz_core::cells::name::CellName;
use bz_core::cells::paths::CellRelativePathBuf;
use bz_core::package::PackageLabel;
use bz_core::provider::label::ProvidersLabel;
use bz_core::provider::label::ProvidersName;
use bz_core::target::label::label::TargetLabel;
use bz_core::target::name::TargetNameRef;

pub(crate) fn bazel_absolute_label_parts(label: &str) -> Option<(String, String)> {
    if let Some((package, target)) = label.rsplit_once(':') {
        if target.is_empty() {
            None
        } else {
            Some((package.to_owned(), target.to_owned()))
        }
    } else {
        label
            .rsplit('/')
            .next()
            .filter(|target| !target.is_empty())
            .map(|target| (label.to_owned(), target.to_owned()))
    }
}

pub(crate) fn parse_bazel_canonical_providers_label(
    label: &str,
    root_cell: CellName,
) -> bz_error::Result<Option<ProvidersLabel>> {
    let Some(label) = label.strip_prefix("@@") else {
        return Ok(None);
    };
    let Some((repo_name, package_and_target)) = label.split_once("//") else {
        return Ok(None);
    };
    let Some((package, target)) = bazel_absolute_label_parts(package_and_target) else {
        return Ok(None);
    };

    let cell_name = if repo_name.is_empty() || repo_name == "root" {
        root_cell
    } else if repo_name == "bazel_tools" {
        CellName::unchecked_new("bazel_tools")?
    } else {
        CellName::unchecked_new(&bzlmod_cell_name(repo_name))?
    };
    let package = PackageLabel::new(cell_name, CellRelativePathBuf::try_from(package)?.as_ref())?;
    let target = TargetNameRef::new_bazel(&target)?;
    Ok(Some(ProvidersLabel::new(
        TargetLabel::new(package, target),
        ProvidersName::Default,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bazel_canonical_label() -> bz_error::Result<()> {
        let label = parse_bazel_canonical_providers_label(
            "@@rules_uv+//uv/private:uv.lock.json",
            CellName::unchecked_new("root")?,
        )?
        .unwrap();
        assert_eq!(
            "bzlmod_rules_uv_//uv/private:uv.lock.json",
            label.to_string()
        );
        Ok(())
    }

    #[test]
    fn parses_bazel_canonical_root_label() -> bz_error::Result<()> {
        let label =
            parse_bazel_canonical_providers_label("@@//foo/bar", CellName::unchecked_new("root")?)?
                .unwrap();
        assert_eq!("root//foo/bar:bar", label.to_string());
        Ok(())
    }
}

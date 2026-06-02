use super::*;

pub async fn bzlmod_module_extension_bazel_usages_digest(
    ctx: &mut DiceComputations<'_>,
    setup: &BzlmodModuleExtensionRepoSetup,
    cancellation: &CancellationContext,
) -> bz_error::Result<String> {
    let extension_cell_path = CellPath::new(
        CellName::unchecked_new(&setup.extension_bzl_cell)?,
        CellRelativePathBuf::try_from(setup.extension_bzl_path.to_string())?,
    );
    let extension_path = ImportPath::new_same_cell(extension_cell_path)?;
    let mut interpreter = ctx
        .get_interpreter_calculator(OwnedStarlarkPath::LoadFile(extension_path.clone()))
        .await?;
    interpreter
        .eval_bzlmod_module_extension_usages_digest(
            &extension_path,
            &setup.extension_usages_json,
            &setup.extension_unique_name,
            &setup.extension_bzl_file,
            &setup.extension_name,
            cancellation,
        )
        .await
}

pub(crate) fn bzlmod_module_extension_bazel_usages_digest_in_eval<'v>(
    extension_usages_json: &str,
    extension_unique_name: &str,
    fallback_extension_bzl_file: &str,
    fallback_extension_name: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<String> {
    let config: BzlmodModuleExtensionEvaluationConfig = serde_json::from_str(extension_usages_json)
        .map_err(|e| {
            bz_error::Error::from(BazelRepositoryError::InvalidModuleExtensionUsageData)
                .context(format!("JSON parse error: {e}"))
        })?;
    let mut expression_index = 0usize;
    let mut json = String::new();
    json.push('{');
    json.push_str("\"extensionUsages\":");
    bzlmod_bazel_usages_extension_usages_json(
        &config.modules,
        fallback_extension_bzl_file,
        fallback_extension_name,
        globals,
        eval,
        &mut expression_index,
        &mut json,
    )?;
    json.push_str(",\"extensionUniqueName\":");
    push_json_string(&mut json, extension_unique_name)?;
    json.push_str(",\"abridgedModules\":");
    bzlmod_bazel_usages_abridged_modules_json(&config.modules, &mut json)?;
    json.push_str(",\"repoMappings\":{}");
    json.push_str(",\"repoOverrides\":");
    bzlmod_bazel_usages_repo_overrides_json(&config.repo_overrides, &mut json)?;
    json.push('}');

    let mut hasher = Sha256::new();
    for code_unit in json.encode_utf16() {
        hasher.update(code_unit.to_le_bytes());
    }
    Ok(BASE64_STANDARD.encode(hasher.finalize()))
}

fn bzlmod_bazel_usages_extension_usages_json<'v>(
    modules: &[BzlmodModuleExtensionModuleConfig],
    fallback_extension_bzl_file: &str,
    fallback_extension_name: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
    expression_index: &mut usize,
    out: &mut String,
) -> starlark::Result<()> {
    out.push('{');
    for (index, module) in modules.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        push_json_string(out, &bzlmod_bazel_module_key(module))?;
        out.push(':');
        bzlmod_bazel_usages_module_usage_json(
            module,
            fallback_extension_bzl_file,
            fallback_extension_name,
            globals,
            eval,
            expression_index,
            out,
        )?;
    }
    out.push('}');
    Ok(())
}

fn bzlmod_bazel_usages_module_usage_json<'v>(
    module: &BzlmodModuleExtensionModuleConfig,
    fallback_extension_bzl_file: &str,
    fallback_extension_name: &str,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
    expression_index: &mut usize,
    out: &mut String,
) -> starlark::Result<()> {
    let extension_bzl_file = if module.extension_bzl_file.is_empty() {
        fallback_extension_bzl_file
    } else {
        &module.extension_bzl_file
    };
    let extension_name = if module.extension_name.is_empty() {
        fallback_extension_name
    } else {
        &module.extension_name
    };
    out.push('{');
    out.push_str("\"extensionBzlFile\":");
    push_json_string(out, extension_bzl_file)?;
    out.push_str(",\"extensionName\":");
    push_json_string(out, extension_name)?;
    out.push_str(",\"proxies\":[]");
    out.push_str(",\"tags\":");
    bzlmod_bazel_usages_tags_json(module, globals, eval, expression_index, out)?;
    out.push_str(",\"repoOverrides\":{}");
    out.push('}');
    Ok(())
}

fn bzlmod_bazel_usages_tags_json<'v>(
    module: &BzlmodModuleExtensionModuleConfig,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
    expression_index: &mut usize,
    out: &mut String,
) -> starlark::Result<()> {
    out.push('[');
    for (index, tag) in module.tags.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"tagName\":");
        push_json_string(out, &tag.tag_name)?;
        out.push_str(",\"attributeValues\":");
        bzlmod_bazel_usages_attribute_values_json(
            module,
            tag,
            globals,
            eval,
            expression_index,
            out,
        )?;
        out.push_str(",\"devDependency\":");
        out.push_str(if tag.dev_dependency { "true" } else { "false" });
        out.push_str(",\"location\":{\"file\":\"<builtin>\",\"line\":0,\"column\":0}");
        out.push('}');
    }
    out.push(']');
    Ok(())
}

fn bzlmod_bazel_usages_attribute_values_json<'v>(
    module: &BzlmodModuleExtensionModuleConfig,
    tag: &BzlmodModuleExtensionTagConfig,
    globals: &Globals,
    eval: &mut Evaluator<'v, '_, '_>,
    expression_index: &mut usize,
    out: &mut String,
) -> starlark::Result<()> {
    let mut expression_bindings = module.constants.clone();
    expression_bindings.extend(tag.bindings.iter().cloned());
    out.push('{');
    for (index, (attr_name, expression)) in tag.kwargs.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        push_json_string(out, attr_name)?;
        out.push(':');
        let value_name = format!("buck_bzlmod_usage_digest_tag_value_{expression_index}");
        *expression_index += 1;
        let raw_value = eval_bzlmod_tag_expression(
            expression,
            &expression_bindings,
            &value_name,
            globals,
            eval,
        )?;
        bzlmod_bazel_usages_attribute_value_json(raw_value, out)?;
    }
    out.push('}');
    Ok(())
}

fn bzlmod_bazel_usages_attribute_value_json(
    value: Value<'_>,
    out: &mut String,
) -> starlark::Result<()> {
    if value.is_none() {
        out.push_str("null");
        return Ok(());
    }
    if let Some(value) = value.unpack_bool() {
        out.push_str(if value { "true" } else { "false" });
        return Ok(());
    }
    if let Some(value) = value.unpack_i32() {
        out.push_str(&value.to_string());
        return Ok(());
    }
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        let label = bazel_canonical_starlark_label_string(&label)?;
        push_json_string(out, &label)?;
        return Ok(());
    }
    if let Some(value) = value.unpack_str() {
        push_json_string(out, &bzlmod_bazel_usages_attribute_string(value))?;
        return Ok(());
    }
    if let Some(dict) = DictRef::from_value(value) {
        out.push('{');
        for (index, (key, value)) in dict.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            let key = bzlmod_bazel_usages_attribute_key_string(key)?;
            push_json_string(out, &key)?;
            out.push(':');
            bzlmod_bazel_usages_attribute_value_json(value, out)?;
        }
        out.push('}');
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        out.push('[');
        for (index, value) in list.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            bzlmod_bazel_usages_attribute_value_json(value, out)?;
        }
        out.push(']');
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        out.push('[');
        for (index, value) in tuple.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            bzlmod_bazel_usages_attribute_value_json(value, out)?;
        }
        out.push(']');
        return Ok(());
    }
    Err(bz_error::bz_error!(
        bz_error::ErrorTag::Input,
        "unsupported bzlmod module extension tag value `{}` of type `{}` for Bazel usagesDigest",
        value.to_repr(),
        value.get_type()
    )
    .into())
}

fn bzlmod_bazel_usages_attribute_key_string(value: Value<'_>) -> starlark::Result<String> {
    if let Some(label) = StarlarkProvidersLabel::from_value(value) {
        return bazel_canonical_starlark_label_string(&label);
    }
    if let Some(value) = value.unpack_str() {
        return Ok(bzlmod_bazel_usages_attribute_string(value));
    }
    Err(bz_error::bz_error!(
        bz_error::ErrorTag::Input,
        "unsupported bzlmod module extension tag dict key `{}` of type `{}` for Bazel usagesDigest",
        value.to_repr(),
        value.get_type()
    )
    .into())
}

fn bzlmod_bazel_usages_attribute_string(value: &str) -> String {
    if value.starts_with("@@") || (value.starts_with('\'') && value.ends_with('\'')) {
        format!("'{value}'")
    } else {
        value.to_owned()
    }
}

fn bzlmod_bazel_usages_abridged_modules_json(
    modules: &[BzlmodModuleExtensionModuleConfig],
    out: &mut String,
) -> starlark::Result<()> {
    out.push('[');
    for (index, module) in modules.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"name\":");
        push_json_string(out, &module.name)?;
        out.push_str(",\"version\":");
        push_json_string(out, &module.version)?;
        out.push_str(",\"key\":");
        push_json_string(out, &bzlmod_bazel_module_key(module))?;
        out.push('}');
    }
    out.push(']');
    Ok(())
}

fn bzlmod_bazel_usages_repo_overrides_json(
    repo_overrides: &[(String, String)],
    out: &mut String,
) -> starlark::Result<()> {
    out.push('{');
    for (index, (repo_name, canonical_repo_name)) in repo_overrides.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        push_json_string(out, repo_name)?;
        out.push(':');
        push_json_string(out, canonical_repo_name)?;
    }
    out.push('}');
    Ok(())
}

fn bzlmod_bazel_module_key(module: &BzlmodModuleExtensionModuleConfig) -> String {
    if module.is_root {
        "<root>".to_owned()
    } else if module.version.is_empty() {
        format!("{}@_", module.name)
    } else {
        format!("{}@{}", module.name, module.version)
    }
}

fn push_json_string(out: &mut String, value: &str) -> starlark::Result<()> {
    let encoded = serde_json::to_string(value).map_err(|e| {
        bz_error::bz_error!(
            bz_error::ErrorTag::Tier0,
            "failed to serialize Bazel lockfile string: {e}"
        )
    })?;
    out.push_str(&encoded);
    Ok(())
}

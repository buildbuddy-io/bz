use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use buck2_build_api_derive::internal_provider;
use buck2_core::cells::cell_path::CellPath;
use buck2_core::cells::external::bzlmod_cell_name;
use buck2_core::cells::name::CellName;
use buck2_core::cells::paths::CellRelativePathBuf;
use buck2_core::fs::buck_out_path::BazelOutputPathKind;
use buck2_core::fs::buck_out_path::BazelOutputRoot;
use buck2_core::fs::buck_out_path::BuckOutPathKind;
use buck2_core::provider::id::ProviderId;
use buck2_execute::execute::request::OutputType;
use buck2_hash::buck_indexset;
use buck2_interpreter::types::configured_providers_label::StarlarkConfiguredProvidersLabel;
use buck2_interpreter::types::configured_providers_label::StarlarkProvidersLabel;
use buck2_interpreter::types::provider::callable::ProviderCallableLike;
use buck2_util::late_binding::LateBinding;
use dupe::Dupe;
use serde::Serializer;
use sha2::Digest;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::StringValueLike;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::ValueOfUnchecked;
use starlark::values::ValueOfUncheckedGeneric;
use starlark::values::ValueTyped;
use starlark::values::dict::AllocDict;
use starlark::values::dict::DictRef;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::AllocTuple;
use starlark::values::tuple::TupleRef;
use starlark::values::tuple::UnpackTuple;
use starlark_map::StarlarkHasher;

use crate as buck2_build_api;
use crate::actions::impls::solib_symlink::UnregisteredSolibSymlinkAction;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::StarlarkInputArtifactLike;
use crate::interpreter::rule_defs::artifact::starlark_artifact_like::ValueAsInputArtifactLikeUnpack;
use crate::interpreter::rule_defs::artifact::starlark_declared_artifact::StarlarkDeclaredArtifact;
use crate::interpreter::rule_defs::bazel::depset::BazelDepset;
use crate::interpreter::rule_defs::bazel::depset::bazel_depset_to_list;
use crate::interpreter::rule_defs::cmd_args::StarlarkCmdArgs;
use crate::interpreter::rule_defs::context::AnalysisActions;
use crate::interpreter::rule_defs::context::analysis_actions_to_bazel_ctx;
use crate::interpreter::rule_defs::provider::ProviderLike;
use crate::interpreter::rule_defs::provider::callable::provider_callable_equals;
use crate::interpreter::rule_defs::provider::callable::provider_callable_write_hash;

const DEBUG_PACKAGE_INFO: &str = "DebugPackageInfo";
const CC_TOOLCHAIN_CONFIG_INFO: &str = "CcToolchainConfigInfo";
const CC_SHARED_LIBRARY_INFO: &str = "CcSharedLibraryInfo";
const CC_SHARED_LIBRARY_HINT_INFO: &str = "CcSharedLibraryHintInfo";
const CC_TOOLCHAIN_INFO: &str = "CcToolchainInfo";
const BAZEL_LINKER_PARAM_FILE_VARIABLE: &str = "linker_param_file";
const BAZEL_LINKER_PARAM_FILE_PLACEHOLDER: &str = "LINKER_PARAM_FILE_PLACEHOLDER";
const BAZEL_CC_ALL_COMPILE_ACTIONS: &[&str] = &[
    "c-compile",
    "c++-compile",
    "c++-header-parsing",
    "c++-module-compile",
    "c++-module-codegen",
    "c++-module-deps-scanning",
    "c++20-module-compile",
    "c++20-module-codegen",
    "assemble",
    "preprocess-assemble",
    "clif-match",
    "linkstamp-compile",
    "cc-flags-make-variable",
    "lto-backend",
    "c++-header-analysis",
];
const BAZEL_CC_ALL_LINK_ACTIONS: &[&str] = &[
    "lto-index-for-executable",
    "lto-index-for-dynamic-library",
    "lto-index-for-nodeps-dynamic-library",
    "c++-link-executable",
    "c++-link-dynamic-library",
    "c++-link-nodeps-dynamic-library",
];
const BAZEL_CC_ALL_ARCHIVE_ACTIONS: &[&str] = &["c++-link-static-library"];
const BAZEL_CC_ALL_OTHER_ACTIONS: &[&str] = &["strip"];
const BAZEL_CC_OBJC_ACTIONS: &[&str] = &[
    "objc-compile",
    "objc++-compile",
    "objc-fully-link",
    "objc-executable",
];

fn rules_cc_provider_path(path: &str) -> CellPath {
    CellPath::new(
        CellName::unchecked_new(&bzlmod_cell_name("rules_cc+"))
            .expect("rules_cc bzlmod cell name should be valid"),
        CellRelativePathBuf::unchecked_new(path.to_owned()),
    )
}

fn cc_native_provider_path(name: &str) -> Option<CellPath> {
    match name {
        // Bazel's C++ builtins wrap the bzlmod rules_cc providers for these
        // symbols, so their provider identity is the @rules_cc+ .bzl file.
        DEBUG_PACKAGE_INFO => Some(rules_cc_provider_path("cc/private/debug_package_info.bzl")),
        CC_TOOLCHAIN_CONFIG_INFO => Some(rules_cc_provider_path(
            "cc/private/toolchain_config/cc_toolchain_config_info.bzl",
        )),
        CC_TOOLCHAIN_INFO => Some(rules_cc_provider_path(
            "cc/private/rules_impl/cc_toolchain_info.bzl",
        )),
        _ => None,
    }
}

#[internal_provider(cc_info_creator)]
#[derive(Clone, Debug, Trace, Coerce, Freeze, ProvidesStaticType, Allocative)]
#[repr(C)]
pub struct CcInfoGen<V: ValueLifetimeless> {
    compilation_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    linking_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    debug_context: ValueOfUncheckedGeneric<V, FrozenValue>,
    cc_native_library_info: ValueOfUncheckedGeneric<V, FrozenValue>,
}

#[starlark_module]
fn cc_info_creator(globals: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenCcInfo)]
    fn CcInfo<'v>(
        #[starlark(require = named, default = NoneType)] compilation_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] linking_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] debug_context: Value<'v>,
        #[starlark(require = named, default = NoneType)] cc_native_library_info: Value<'v>,
    ) -> starlark::Result<CcInfo<'v>> {
        Ok(CcInfo {
            compilation_context: ValueOfUnchecked::<FrozenValue>::new(compilation_context),
            linking_context: ValueOfUnchecked::<FrozenValue>::new(linking_context),
            debug_context: ValueOfUnchecked::<FrozenValue>::new(debug_context),
            cc_native_library_info: ValueOfUnchecked::<FrozenValue>::new(cc_native_library_info),
        })
    }
}

#[derive(Clone, Debug, Freeze, ProvidesStaticType, Trace, Allocative)]
#[repr(C)]
pub struct CcNativeProviderGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[trace(unsafe_ignore)]
    id: Arc<ProviderId>,
    #[freeze(identity)]
    name: &'static str,
    values: Box<[(String, V)]>,
}

starlark::starlark_complex_value!(pub CcNativeProvider);

unsafe impl<FromV, ToV> Coerce<CcNativeProviderGen<ToV>> for CcNativeProviderGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for CcNativeProviderGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(<{} field(s)>)", self.name, self.values.len())
    }
}

#[derive(
    Clone,
    Debug,
    Freeze,
    ProvidesStaticType,
    Trace,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct BazelCcToolchainVariablesGen<V: ValueLifetimeless> {
    parent: Option<V>,
    values: Box<[(String, V)]>,
}

starlark::starlark_complex_value!(pub BazelCcToolchainVariables);

unsafe impl<FromV, ToV> Coerce<BazelCcToolchainVariablesGen<ToV>>
    for BazelCcToolchainVariablesGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for BazelCcToolchainVariablesGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<CcToolchainVariables>")
    }
}

#[starlark_value(type = "CcToolchainVariables")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelCcToolchainVariablesGen<V> where
    Self: ProvidesStaticType<'v>
{
}

#[derive(
    Clone,
    Debug,
    Freeze,
    ProvidesStaticType,
    Trace,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct BazelCcCompileCommandLineGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[trace(unsafe_ignore)]
    feature_configuration: Arc<BazelFeatureConfigurationData>,
    action_name: String,
    variables: ValueOfUncheckedGeneric<V, FrozenValue>,
}

starlark::starlark_complex_value!(pub BazelCcCompileCommandLine);

unsafe impl<FromV, ToV> Coerce<BazelCcCompileCommandLineGen<ToV>>
    for BazelCcCompileCommandLineGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for BazelCcCompileCommandLineGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<CcCompileCommandLine({})>", self.action_name)
    }
}

#[starlark_value(type = "CcCompileCommandLine")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelCcCompileCommandLineGen<V> where
    Self: ProvidesStaticType<'v>
{
}

impl<V: ValueLifetimeless> BazelCcCompileCommandLineGen<V> {
    pub fn visit_argument_strings<'v>(
        &self,
        heap: Heap<'v>,
        visit: &mut impl FnMut(String) -> starlark::Result<()>,
    ) -> starlark::Result<()>
    where
        V: ValueLike<'v>,
    {
        let feature_configuration = BazelFeatureConfiguration {
            requested_features: Vec::new(),
            data: Arc::clone(&self.feature_configuration),
        };
        bazel_cc_feature_command_line_visit(
            &feature_configuration,
            &self.action_name,
            self.variables.get().to_value(),
            heap,
            visit,
        )
    }

    pub fn argument_strings<'v>(&self, heap: Heap<'v>) -> starlark::Result<Vec<String>>
    where
        V: ValueLike<'v>,
    {
        let mut args = Vec::new();
        self.visit_argument_strings(heap, &mut |arg| {
            args.push(arg);
            Ok(())
        })?;
        Ok(args)
    }

    pub fn environment_strings<'v>(&self, heap: Heap<'v>) -> starlark::Result<Vec<(String, String)>>
    where
        V: ValueLike<'v>,
    {
        let feature_configuration = BazelFeatureConfiguration {
            requested_features: Vec::new(),
            data: Arc::clone(&self.feature_configuration),
        };
        bazel_cc_feature_environment_strings(
            &feature_configuration,
            &self.action_name,
            self.variables.get().to_value(),
            heap,
        )
    }
}

impl<'v, V: ValueLike<'v>> BazelCcToolchainVariablesGen<V> {
    fn local_value(&self, name: &str) -> Option<Value<'v>> {
        self.values
            .binary_search_by(|(key, _)| key.as_str().cmp(name))
            .ok()
            .map(|index| self.values[index].1.to_value())
    }

    fn value(&self, name: &str) -> Option<Value<'v>> {
        if let Some(value) = self.local_value(name) {
            return Some(value);
        }
        let parent = self.parent?.to_value();
        BazelCcToolchainVariables::from_value(parent)
            .and_then(|parent| parent.value(name))
            .or_else(|| bazel_cc_build_variable_from_dict(parent, name))
    }

    fn local_values(&self) -> impl Iterator<Item = (&str, Value<'v>)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_value()))
    }
}

impl<'v, V: ValueLike<'v>> serde::Serialize for CcNativeProviderGen<V> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.collect_map(
            self.values
                .iter()
                .map(|(name, value)| (name, value.to_value())),
        )
    }
}

#[starlark_value(type = "CcNativeProvider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CcNativeProviderGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn dir_attr(&self) -> Vec<String> {
        self.values.iter().map(|(name, _)| name.clone()).collect()
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.values
            .binary_search_by(|(name, _)| name.as_str().cmp(attribute))
            .ok()
            .map(|index| self.values[index].1.to_value())
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderLike>(self);
    }
}

impl<'v, V: ValueLike<'v>> ProviderLike<'v> for CcNativeProviderGen<V> {
    fn id(&self) -> &Arc<ProviderId> {
        &self.id
    }

    fn items(&self) -> Vec<(&str, Value<'v>)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_value()))
            .collect()
    }
}

#[derive(Debug, Clone, Dupe, ProvidesStaticType, NoSerialize, Allocative)]
struct CcNativeProviderCallable {
    name: &'static str,
    id: Arc<ProviderId>,
}

impl CcNativeProviderCallable {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            id: Arc::new(ProviderId {
                path: cc_native_provider_path(name),
                name: name.to_owned(),
            }),
        }
    }
}

impl fmt::Display for CcNativeProviderCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

starlark::starlark_simple_value!(CcNativeProviderCallable);

#[starlark_value(type = "cc_native_provider_callable")]
impl<'v> StarlarkValue<'v> for CcNativeProviderCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        Ok(eval
            .heap()
            .alloc(make_cc_native_provider_from_kwargs(
                self.name,
                self.id.dupe(),
                args.names_map()?,
            ))
            .to_value())
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderCallableLike>(self);
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        provider_callable_equals(self, other)
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        provider_callable_write_hash(self, hasher)
    }
}

impl ProviderCallableLike for CcNativeProviderCallable {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>> {
        Ok(&self.id)
    }
}

fn make_cc_native_provider_from_kwargs<'v>(
    name: &'static str,
    id: Arc<ProviderId>,
    kwargs: SmallMap<starlark::values::StringValue<'v>, Value<'v>>,
) -> CcNativeProvider<'v> {
    make_cc_native_provider(
        name,
        id,
        kwargs
            .into_iter()
            .map(|(name, value)| (name.as_str().to_owned(), value)),
    )
}

fn make_cc_native_provider<'v>(
    name: &'static str,
    id: Arc<ProviderId>,
    values: impl IntoIterator<Item = (String, Value<'v>)>,
) -> CcNativeProvider<'v> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by(|(left, _), (right, _)| left.cmp(right));
    CcNativeProvider {
        id,
        name,
        values: values.into_boxed_slice(),
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcInternal;

impl fmt::Display for BazelCcInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cc_internal")
    }
}

starlark::starlark_simple_value!(BazelCcInternal);

pub struct BazelCcCompileAction<'v> {
    pub actions: ValueTyped<'v, AnalysisActions<'v>>,
    pub executable: Value<'v>,
    pub command_line: ValueTyped<'v, BazelCcCompileCommandLine<'v>>,
    pub inputs: StarlarkCmdArgs<'v>,
    pub outputs: Vec<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>,
    pub mnemonic: StringValue<'v>,
}

pub static BAZEL_CC_CREATE_COMPILE_ACTION: LateBinding<
    for<'v, 'a, 'b, 'c> fn(
        BazelCcCompileAction<'v>,
        &'a mut Evaluator<'v, 'b, 'c>,
    ) -> starlark::Result<NoneType>,
> = LateBinding::new("BAZEL_CC_CREATE_COMPILE_ACTION");

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelCcToolchainFeatures {
    selectables: Vec<BazelSelectable>,
    default_selectables: Vec<String>,
    action_tools: Arc<Vec<BazelActionTool>>,
    flag_sets: Arc<Vec<BazelFlagSet>>,
    env_sets: Arc<Vec<BazelEnvSet>>,
    artifact_name_patterns: Vec<BazelArtifactNamePattern>,
    tools_directory: String,
    #[allocative(skip)]
    feature_configuration_cache: Mutex<HashMap<Vec<String>, Arc<BazelFeatureConfigurationData>>>,
}

impl fmt::Display for BazelCcToolchainFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<CcToolchainFeatures>")
    }
}

starlark::starlark_simple_value!(BazelCcToolchainFeatures);

#[starlark_value(type = "CcToolchainFeatures")]
impl<'v> StarlarkValue<'v> for BazelCcToolchainFeatures {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_toolchain_features_methods)
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelFeatureConfiguration {
    requested_features: Vec<String>,
    data: Arc<BazelFeatureConfigurationData>,
}

#[derive(Debug, Allocative)]
struct BazelFeatureConfigurationData {
    enabled_selectable_set: HashSet<String>,
    action_tools: Arc<Vec<BazelActionTool>>,
    action_config_flag_sets: Vec<BazelFlagSet>,
    feature_flag_sets: Vec<BazelFlagSet>,
    env_sets: Vec<BazelEnvSet>,
    selected_action_tools: HashMap<String, BazelActionTool>,
    action_config_flag_sets_by_action: HashMap<String, Vec<usize>>,
    feature_flag_sets_by_action: HashMap<String, Vec<usize>>,
    env_sets_by_action: HashMap<String, Vec<usize>>,
    tools_directory: String,
}

impl fmt::Display for BazelFeatureConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<FeatureConfiguration({})>",
            self.requested_features.join(", ")
        )
    }
}

starlark::starlark_simple_value!(BazelFeatureConfiguration);

#[derive(Debug, Clone, Allocative)]
struct BazelSelectable {
    name: String,
    requires: Vec<Vec<String>>,
    implies: Vec<String>,
    provides: Vec<String>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelActionTool {
    action_name: String,
    path: String,
    path_origin: BazelToolPathOrigin,
    with_features: Vec<BazelWithFeatureSet>,
    execution_requirements: Vec<String>,
}

#[derive(Debug, Clone, Allocative)]
enum BazelToolPathOrigin {
    CrosstoolPackage,
    FilesystemRoot,
    WorkspaceRoot,
}

#[derive(Debug, Clone, Allocative)]
struct BazelWithFeatureSet {
    features: Vec<String>,
    not_features: Vec<String>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelArtifactNamePattern {
    category: String,
    prefix: String,
    extension: String,
}

#[derive(Debug, Clone, Allocative)]
struct BazelFlagSet {
    owner_selectable: String,
    owner_is_action_config: bool,
    actions: Vec<String>,
    with_features: Vec<BazelWithFeatureSet>,
    flag_groups: Vec<BazelFlagGroup>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelEnvSet {
    owner_selectable: String,
    actions: Vec<String>,
    with_features: Vec<BazelWithFeatureSet>,
    env_entries: Vec<BazelEnvEntry>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelEnvEntry {
    key: String,
    value: String,
    expand_if_available: Option<String>,
}

#[derive(Debug, Clone, Allocative)]
struct BazelFlagGroup {
    flags: Vec<String>,
    flag_groups: Vec<BazelFlagGroup>,
    iterate_over: Option<String>,
    expand_if_available: Option<String>,
    expand_if_not_available: Option<String>,
    expand_if_true: Option<String>,
    expand_if_false: Option<String>,
    expand_if_equal: Option<(String, String)>,
}

#[derive(Debug)]
struct BazelArtifactCategory {
    name: &'static str,
    default_prefix: &'static str,
    default_extension: &'static str,
    allowed_extensions: &'static [&'static str],
}

const BAZEL_CC_ARTIFACT_CATEGORIES: &[BazelArtifactCategory] = &[
    BazelArtifactCategory {
        name: "static_library",
        default_prefix: "lib",
        default_extension: ".a",
        allowed_extensions: &[".a", ".lib"],
    },
    BazelArtifactCategory {
        name: "alwayslink_static_library",
        default_prefix: "lib",
        default_extension: ".lo",
        allowed_extensions: &[".lo", ".lo.lib"],
    },
    BazelArtifactCategory {
        name: "dynamic_library",
        default_prefix: "lib",
        default_extension: ".so",
        allowed_extensions: &[".so", ".dylib", ".dll", ".pyd", ".wasm"],
    },
    BazelArtifactCategory {
        name: "executable",
        default_prefix: "",
        default_extension: "",
        allowed_extensions: &["", ".exe", ".wasm"],
    },
    BazelArtifactCategory {
        name: "interface_library",
        default_prefix: "lib",
        default_extension: ".ifso",
        allowed_extensions: &[".ifso", ".tbd", ".if.lib", ".lib"],
    },
    BazelArtifactCategory {
        name: "pic_file",
        default_prefix: "",
        default_extension: ".pic",
        allowed_extensions: &[".pic"],
    },
    BazelArtifactCategory {
        name: "included_file_list",
        default_prefix: "",
        default_extension: ".d",
        allowed_extensions: &[".d"],
    },
    BazelArtifactCategory {
        name: "serialized_diagnostics_file",
        default_prefix: "",
        default_extension: ".dia",
        allowed_extensions: &[".dia"],
    },
    BazelArtifactCategory {
        name: "object_file",
        default_prefix: "",
        default_extension: ".o",
        allowed_extensions: &[".o", ".obj"],
    },
    BazelArtifactCategory {
        name: "pic_object_file",
        default_prefix: "",
        default_extension: ".pic.o",
        allowed_extensions: &[".pic.o"],
    },
    BazelArtifactCategory {
        name: "cpp_module",
        default_prefix: "",
        default_extension: ".pcm",
        allowed_extensions: &[".pcm", ".gcm", ".ifc"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_info",
        default_prefix: "",
        default_extension: ".CXXModules.json",
        allowed_extensions: &[".CXXModules.json"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_ddi",
        default_prefix: "",
        default_extension: ".ddi",
        allowed_extensions: &[".ddi"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_modmap",
        default_prefix: "",
        default_extension: ".modmap",
        allowed_extensions: &[".modmap"],
    },
    BazelArtifactCategory {
        name: "cpp_modules_modmap_input",
        default_prefix: "",
        default_extension: ".modmap.input",
        allowed_extensions: &[".modmap.input"],
    },
    BazelArtifactCategory {
        name: "generated_assembly",
        default_prefix: "",
        default_extension: ".s",
        allowed_extensions: &[".s", ".asm"],
    },
    BazelArtifactCategory {
        name: "processed_header",
        default_prefix: "",
        default_extension: ".processed",
        allowed_extensions: &[".processed"],
    },
    BazelArtifactCategory {
        name: "generated_header",
        default_prefix: "",
        default_extension: ".h",
        allowed_extensions: &[".h"],
    },
    BazelArtifactCategory {
        name: "preprocessed_c_source",
        default_prefix: "",
        default_extension: ".i",
        allowed_extensions: &[".i"],
    },
    BazelArtifactCategory {
        name: "preprocessed_cpp_source",
        default_prefix: "",
        default_extension: ".ii",
        allowed_extensions: &[".ii"],
    },
    BazelArtifactCategory {
        name: "coverage_data_file",
        default_prefix: "",
        default_extension: ".gcno",
        allowed_extensions: &[".gcno"],
    },
    BazelArtifactCategory {
        name: "clif_output_proto",
        default_prefix: "",
        default_extension: ".opb",
        allowed_extensions: &[".opb"],
    },
];

fn bazel_cc_error(message: impl Into<String>) -> starlark::Error {
    starlark::Error::new_other(std::io::Error::other(message.into()))
}

fn bazel_cc_sequence_values<'v>(value: Value<'v>, field: &str) -> starlark::Result<Vec<Value<'v>>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    if let Some(list) = ListRef::from_value(value) {
        return Ok(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Ok(tuple.iter().collect());
    }
    Err(bazel_cc_error(format!(
        "Expected `{field}` to be a list or tuple, got `{}`",
        value.get_type()
    )))
}

fn bazel_cc_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Option<Value<'v>>> {
    value.get_attr(name, heap)
}

fn bazel_cc_string_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Option<String>> {
    let Some(attr) = bazel_cc_attr(value, name, heap)? else {
        return Ok(None);
    };
    if attr.is_none() {
        return Ok(None);
    }
    attr.unpack_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected `{name}` to be a string, got `{}`",
                attr.get_type()
            ))
        })
}

fn bazel_cc_bool_attr<'v>(value: Value<'v>, name: &str, heap: Heap<'v>) -> starlark::Result<bool> {
    let Some(attr) = bazel_cc_attr(value, name, heap)? else {
        return Ok(false);
    };
    if attr.is_none() {
        return Ok(false);
    }
    attr.unpack_bool().ok_or_else(|| {
        bazel_cc_error(format!(
            "Expected `{name}` to be a bool, got `{}`",
            attr.get_type()
        ))
    })
}

fn bazel_cc_string_sequence<'v>(value: Value<'v>, field: &str) -> starlark::Result<Vec<String>> {
    bazel_cc_sequence_values(value, field)?
        .into_iter()
        .map(|value| {
            value
                .unpack_str()
                .map(|value| value.to_owned())
                .ok_or_else(|| {
                    bazel_cc_error(format!(
                        "Expected `{field}` entries to be strings, got `{}`",
                        value.get_type()
                    ))
                })
        })
        .collect()
}

fn bazel_cc_string_sequence_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Vec<String>> {
    let Some(attr) = bazel_cc_attr(value, name, heap)? else {
        return Ok(Vec::new());
    };
    bazel_cc_string_sequence(attr, name)
}

fn bazel_cc_push_unique(values: &mut Vec<String>, value: String) -> bool {
    if values.iter().any(|existing| existing == &value) {
        false
    } else {
        values.push(value);
        true
    }
}

fn bazel_cc_parse_with_feature_set<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<BazelWithFeatureSet> {
    Ok(BazelWithFeatureSet {
        features: bazel_cc_string_sequence_attr(value, "features", heap)?,
        not_features: bazel_cc_string_sequence_attr(value, "not_features", heap)?,
    })
}

fn bazel_cc_parse_requires<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Vec<String>>> {
    let Some(requires) = bazel_cc_attr(value, "requires", heap)? else {
        return Ok(Vec::new());
    };
    bazel_cc_sequence_values(requires, "requires")?
        .into_iter()
        .map(|feature_set| bazel_cc_string_sequence_attr(feature_set, "features", heap))
        .collect()
}

fn bazel_cc_parse_expand_if_equal<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Option<(String, String)>> {
    let Some(name) = bazel_cc_string_attr(value, "name", heap)? else {
        return Ok(None);
    };
    let value = bazel_cc_string_attr(value, "value", heap)?.ok_or_else(|| {
        bazel_cc_error("Expected variable_with_value to expose a `value` attribute")
    })?;
    Ok(Some((name, value)))
}

fn bazel_cc_parse_optional_string_attr<'v>(
    value: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Option<String>> {
    bazel_cc_string_attr(value, name, heap)
}

fn bazel_cc_parse_flag_group<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<BazelFlagGroup> {
    let flag_groups = if let Some(flag_groups) = bazel_cc_attr(value, "flag_groups", heap)? {
        bazel_cc_sequence_values(flag_groups, "flag_groups")?
            .into_iter()
            .map(|value| bazel_cc_parse_flag_group(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let expand_if_equal = if let Some(value) = bazel_cc_attr(value, "expand_if_equal", heap)? {
        if value.is_none() {
            None
        } else {
            bazel_cc_parse_expand_if_equal(value, heap)?
        }
    } else {
        None
    };
    Ok(BazelFlagGroup {
        flags: bazel_cc_string_sequence_attr(value, "flags", heap)?,
        flag_groups,
        iterate_over: bazel_cc_parse_optional_string_attr(value, "iterate_over", heap)?,
        expand_if_available: bazel_cc_parse_optional_string_attr(
            value,
            "expand_if_available",
            heap,
        )?,
        expand_if_not_available: bazel_cc_parse_optional_string_attr(
            value,
            "expand_if_not_available",
            heap,
        )?,
        expand_if_true: bazel_cc_parse_optional_string_attr(value, "expand_if_true", heap)?,
        expand_if_false: bazel_cc_parse_optional_string_attr(value, "expand_if_false", heap)?,
        expand_if_equal,
    })
}

fn bazel_cc_parse_flag_set<'v>(
    owner_selectable: &str,
    owner_is_action_config: bool,
    action_name: Option<&str>,
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<BazelFlagSet> {
    let mut actions = bazel_cc_string_sequence_attr(value, "actions", heap)?;
    if actions.is_empty() {
        if let Some(action_name) = action_name {
            actions.push(action_name.to_owned());
        }
    }
    let with_features = if let Some(value) = bazel_cc_attr(value, "with_features", heap)? {
        bazel_cc_sequence_values(value, "with_features")?
            .into_iter()
            .map(|value| bazel_cc_parse_with_feature_set(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let flag_groups = if let Some(value) = bazel_cc_attr(value, "flag_groups", heap)? {
        bazel_cc_sequence_values(value, "flag_groups")?
            .into_iter()
            .map(|value| bazel_cc_parse_flag_group(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok(BazelFlagSet {
        owner_selectable: owner_selectable.to_owned(),
        owner_is_action_config,
        actions,
        with_features,
        flag_groups,
    })
}

fn bazel_cc_parse_env_entry<'v>(
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<BazelEnvEntry> {
    let key = bazel_cc_string_attr(value, "key", heap)?
        .ok_or_else(|| bazel_cc_error("Expected env_entry key to be a string"))?;
    let value_string = bazel_cc_string_attr(value, "value", heap)?
        .ok_or_else(|| bazel_cc_error("Expected env_entry value to be a string"))?;
    Ok(BazelEnvEntry {
        key,
        value: value_string,
        expand_if_available: bazel_cc_string_attr(value, "expand_if_available", heap)?,
    })
}

fn bazel_cc_parse_env_set<'v>(
    owner_selectable: &str,
    value: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<BazelEnvSet> {
    let actions = bazel_cc_string_sequence_attr(value, "actions", heap)?;
    let with_features = if let Some(value) = bazel_cc_attr(value, "with_features", heap)? {
        bazel_cc_sequence_values(value, "with_features")?
            .into_iter()
            .map(|value| bazel_cc_parse_with_feature_set(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let env_entries = if let Some(value) = bazel_cc_attr(value, "env_entries", heap)? {
        bazel_cc_sequence_values(value, "env_entries")?
            .into_iter()
            .map(|value| bazel_cc_parse_env_entry(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok(BazelEnvSet {
        owner_selectable: owner_selectable.to_owned(),
        actions,
        with_features,
        env_entries,
    })
}

fn bazel_cc_parse_tool<'v>(
    action_name: &str,
    tool: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<BazelActionTool> {
    let (path, path_origin) = if let Some(path) = bazel_cc_string_attr(tool, "path", heap)? {
        let path_origin = if path.starts_with('/') {
            BazelToolPathOrigin::FilesystemRoot
        } else {
            BazelToolPathOrigin::CrosstoolPackage
        };
        (path, path_origin)
    } else if let Some(tool_artifact) = bazel_cc_attr(tool, "tool", heap)? {
        let path = bazel_cc_string_attr(tool_artifact, "path", heap)?.ok_or_else(|| {
            bazel_cc_error("Expected action_config tool artifact to expose a `path` attribute")
        })?;
        (path, BazelToolPathOrigin::WorkspaceRoot)
    } else {
        return Err(bazel_cc_error(
            "Expected action_config tool to provide exactly one of `path` or `tool`",
        ));
    };

    let with_features = if let Some(value) = bazel_cc_attr(tool, "with_features", heap)? {
        bazel_cc_sequence_values(value, "with_features")?
            .into_iter()
            .map(|value| bazel_cc_parse_with_feature_set(value, heap))
            .collect::<starlark::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };

    Ok(BazelActionTool {
        action_name: action_name.to_owned(),
        path,
        path_origin,
        with_features,
        execution_requirements: bazel_cc_string_sequence_attr(
            tool,
            "execution_requirements",
            heap,
        )?,
    })
}

fn bazel_cc_parse_tool_paths<'v>(
    toolchain_config_info: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<(String, String)>> {
    let Some(tool_paths) = bazel_cc_attr(toolchain_config_info, "tool_paths", heap)? else {
        return Ok(Vec::new());
    };
    let mut parsed = Vec::new();
    for tool_path in bazel_cc_sequence_values(tool_paths, "tool_paths")? {
        let Some(name) = bazel_cc_string_attr(tool_path, "name", heap)? else {
            continue;
        };
        let Some(path) = bazel_cc_string_attr(tool_path, "path", heap)? else {
            continue;
        };
        parsed.push((name, path));
    }
    Ok(parsed)
}

fn bazel_cc_artifact_category(category: &str) -> starlark::Result<&'static BazelArtifactCategory> {
    let category = category.to_ascii_lowercase();
    BAZEL_CC_ARTIFACT_CATEGORIES
        .iter()
        .find(|candidate| candidate.name == category)
        .ok_or_else(|| bazel_cc_error(format!("Artifact category {category} not recognized.")))
}

fn bazel_cc_parse_artifact_name_patterns<'v>(
    toolchain_config_info: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<BazelArtifactNamePattern>> {
    let Some(patterns) = bazel_cc_attr(
        toolchain_config_info,
        "_artifact_name_patterns_DO_NOT_USE",
        heap,
    )?
    else {
        return Ok(Vec::new());
    };

    let mut parsed = Vec::new();
    for pattern in bazel_cc_sequence_values(patterns, "_artifact_name_patterns_DO_NOT_USE")? {
        let category_name =
            bazel_cc_string_attr(pattern, "category_name", heap)?.ok_or_else(|| {
                bazel_cc_error(
                    "The `category_name` field of artifact_name_pattern must be a string.",
                )
            })?;
        if category_name.is_empty() {
            return Err(bazel_cc_error(
                "The `category_name` field of artifact_name_pattern must be a nonempty string.",
            ));
        }
        let category = bazel_cc_artifact_category(&category_name)?;
        let prefix = bazel_cc_string_attr(pattern, "prefix", heap)?.unwrap_or_default();
        let extension = bazel_cc_string_attr(pattern, "extension", heap)?.unwrap_or_default();
        if !category.allowed_extensions.contains(&extension.as_str()) {
            return Err(bazel_cc_error(format!(
                "Unrecognized file extension `{extension}` for artifact category `{}`.",
                category.name
            )));
        }
        if parsed
            .iter()
            .any(|existing: &BazelArtifactNamePattern| existing.category == category.name)
        {
            return Err(bazel_cc_error(format!(
                "Duplicate artifact_name_pattern for category `{}`.",
                category.name
            )));
        }
        if prefix != category.default_prefix || extension != category.default_extension {
            parsed.push(BazelArtifactNamePattern {
                category: category.name.to_owned(),
                prefix,
                extension,
            });
        }
    }

    Ok(parsed)
}

fn bazel_cc_tool_path_origin(path: &str) -> BazelToolPathOrigin {
    if path.starts_with('/') {
        BazelToolPathOrigin::FilesystemRoot
    } else {
        BazelToolPathOrigin::CrosstoolPackage
    }
}

fn bazel_cc_legacy_action_tool(action_name: &str, path: &str) -> BazelActionTool {
    BazelActionTool {
        action_name: action_name.to_owned(),
        path: path.to_owned(),
        path_origin: bazel_cc_tool_path_origin(path),
        with_features: Vec::new(),
        execution_requirements: Vec::new(),
    }
}

fn bazel_cc_add_legacy_action_config(
    selectables: &mut Vec<BazelSelectable>,
    action_tools: &mut Vec<BazelActionTool>,
    existing_action_config_names: &[String],
    action_name: &str,
    tool_path: Option<&str>,
    implies: &[&str],
) {
    if existing_action_config_names
        .iter()
        .any(|existing| existing == action_name)
    {
        return;
    }
    let Some(tool_path) = tool_path else {
        return;
    };
    selectables.push(BazelSelectable {
        name: action_name.to_owned(),
        requires: Vec::new(),
        implies: implies.iter().map(|value| (*value).to_owned()).collect(),
        provides: Vec::new(),
    });
    action_tools.push(bazel_cc_legacy_action_tool(action_name, tool_path));
}

fn bazel_cc_add_legacy_action_configs(
    selectables: &mut Vec<BazelSelectable>,
    action_tools: &mut Vec<BazelActionTool>,
    tool_paths: &[(String, String)],
    existing_action_config_names: &[String],
    add_legacy_feature_implies: bool,
) {
    let tool_path = |name: &str| {
        tool_paths
            .iter()
            .find_map(|(tool_name, path)| (tool_name == name).then_some(path.as_str()))
    };

    let compile_implies: &[&str] = &[];
    let link_implies: &[&str] = if add_legacy_feature_implies {
        &[
            "shared_flag",
            "output_execpath_flags",
            "runtime_library_search_directories",
            "library_search_directories",
            "libraries_to_link",
            "user_link_flags",
        ]
    } else {
        &[]
    };
    let archive_implies: &[&str] = if add_legacy_feature_implies {
        &["archiver_flags"]
    } else {
        &[]
    };

    let gcc = tool_path("gcc");
    for action_name in [
        "assemble",
        "preprocess-assemble",
        "linkstamp-compile",
        "lto-backend",
        "c-compile",
        "c++-compile",
        "c++-header-parsing",
        "c++-module-compile",
        "c++-module-codegen",
    ] {
        bazel_cc_add_legacy_action_config(
            selectables,
            action_tools,
            existing_action_config_names,
            action_name,
            gcc,
            compile_implies,
        );
    }
    for action_name in [
        "c++-link-executable",
        "lto-index-for-executable",
        "c++-link-nodeps-dynamic-library",
        "lto-index-for-nodeps-dynamic-library",
        "c++-link-dynamic-library",
        "lto-index-for-dynamic-library",
    ] {
        bazel_cc_add_legacy_action_config(
            selectables,
            action_tools,
            existing_action_config_names,
            action_name,
            gcc,
            link_implies,
        );
    }
    bazel_cc_add_legacy_action_config(
        selectables,
        action_tools,
        existing_action_config_names,
        "c++-link-static-library",
        tool_path("ar"),
        archive_implies,
    );
    bazel_cc_add_legacy_action_config(
        selectables,
        action_tools,
        existing_action_config_names,
        "strip",
        tool_path("strip"),
        &[],
    );
}

fn bazel_cc_strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn bazel_cc_flag_group(flags: &[&str]) -> BazelFlagGroup {
    BazelFlagGroup {
        flags: bazel_cc_strings(flags),
        flag_groups: Vec::new(),
        iterate_over: None,
        expand_if_available: None,
        expand_if_not_available: None,
        expand_if_true: None,
        expand_if_false: None,
        expand_if_equal: None,
    }
}

fn bazel_cc_nested_flag_group(flag_groups: Vec<BazelFlagGroup>) -> BazelFlagGroup {
    BazelFlagGroup {
        flags: Vec::new(),
        flag_groups,
        iterate_over: None,
        expand_if_available: None,
        expand_if_not_available: None,
        expand_if_true: None,
        expand_if_false: None,
        expand_if_equal: None,
    }
}

fn bazel_cc_feature_flag_set(
    owner_selectable: &str,
    actions: &[&str],
    flag_groups: Vec<BazelFlagGroup>,
) -> BazelFlagSet {
    BazelFlagSet {
        owner_selectable: owner_selectable.to_owned(),
        owner_is_action_config: false,
        actions: bazel_cc_strings(actions),
        with_features: Vec::new(),
        flag_groups,
    }
}

fn bazel_cc_legacy_link_actions() -> &'static [&'static str] {
    &[
        "c++-link-dynamic-library",
        "c++-link-executable",
        "c++-link-nodeps-dynamic-library",
        "lto-index-for-dynamic-library",
        "lto-index-for-executable",
        "lto-index-for-nodeps-dynamic-library",
    ]
}

fn bazel_cc_legacy_dynamic_link_actions() -> &'static [&'static str] {
    &[
        "c++-link-dynamic-library",
        "c++-link-nodeps-dynamic-library",
        "lto-index-for-dynamic-library",
        "lto-index-for-nodeps-dynamic-library",
    ]
}

fn bazel_cc_legacy_archiver_flag_sets(platform: &str) -> Vec<BazelFlagSet> {
    let mut flag_groups = Vec::new();
    flag_groups.push(bazel_cc_flag_group(&[if platform == "mac" {
        "-static"
    } else {
        "rcsD"
    }]));
    let mut output_group = if platform == "mac" {
        bazel_cc_flag_group(&["-o", "%{output_execpath}"])
    } else {
        bazel_cc_flag_group(&["%{output_execpath}"])
    };
    output_group.expand_if_available = Some("output_execpath".to_owned());
    flag_groups.push(output_group);

    let mut object_file_group = bazel_cc_flag_group(&["%{libraries_to_link.name}"]);
    object_file_group.expand_if_equal = Some((
        "libraries_to_link.type".to_owned(),
        "object_file".to_owned(),
    ));
    let mut object_file_group_files = bazel_cc_flag_group(&["%{libraries_to_link.object_files}"]);
    object_file_group_files.iterate_over = Some("libraries_to_link.object_files".to_owned());
    object_file_group_files.expand_if_equal = Some((
        "libraries_to_link.type".to_owned(),
        "object_file_group".to_owned(),
    ));
    let mut libraries =
        bazel_cc_nested_flag_group(vec![object_file_group, object_file_group_files]);
    libraries.iterate_over = Some("libraries_to_link".to_owned());
    libraries.expand_if_available = Some("libraries_to_link".to_owned());
    flag_groups.push(libraries);

    vec![bazel_cc_feature_flag_set(
        "archiver_flags",
        &["c++-link-static-library"],
        flag_groups,
    )]
}

fn bazel_cc_legacy_output_execpath_flag_sets() -> Vec<BazelFlagSet> {
    let mut group = bazel_cc_flag_group(&["-o", "%{output_execpath}"]);
    group.expand_if_available = Some("output_execpath".to_owned());
    vec![bazel_cc_feature_flag_set(
        "output_execpath_flags",
        bazel_cc_legacy_link_actions(),
        vec![group],
    )]
}

fn bazel_cc_legacy_library_search_directories_flag_sets() -> Vec<BazelFlagSet> {
    let mut group = bazel_cc_flag_group(&["-L%{library_search_directories}"]);
    group.iterate_over = Some("library_search_directories".to_owned());
    group.expand_if_available = Some("library_search_directories".to_owned());
    vec![bazel_cc_feature_flag_set(
        "library_search_directories",
        bazel_cc_legacy_link_actions(),
        vec![group],
    )]
}

fn bazel_cc_legacy_runtime_library_search_directories_flag_sets(
    platform: &str,
) -> Vec<BazelFlagSet> {
    let origin = if platform == "mac" {
        "@loader_path"
    } else {
        "$ORIGIN"
    };
    let mut group = bazel_cc_flag_group(&[
        "-Xlinker",
        "-rpath",
        "-Xlinker",
        &format!("{origin}/%{{runtime_library_search_directories}}"),
    ]);
    group.iterate_over = Some("runtime_library_search_directories".to_owned());
    group.expand_if_available = Some("runtime_library_search_directories".to_owned());
    vec![bazel_cc_feature_flag_set(
        "runtime_library_search_directories",
        bazel_cc_legacy_link_actions(),
        vec![group],
    )]
}

fn bazel_cc_legacy_user_link_flags_flag_sets() -> Vec<BazelFlagSet> {
    let mut group = bazel_cc_flag_group(&["%{user_link_flags}"]);
    group.iterate_over = Some("user_link_flags".to_owned());
    group.expand_if_available = Some("user_link_flags".to_owned());
    vec![bazel_cc_feature_flag_set(
        "user_link_flags",
        bazel_cc_legacy_link_actions(),
        vec![group],
    )]
}

fn bazel_cc_legacy_shared_flag_sets() -> Vec<BazelFlagSet> {
    vec![bazel_cc_feature_flag_set(
        "shared_flag",
        bazel_cc_legacy_dynamic_link_actions(),
        vec![bazel_cc_flag_group(&["-shared"])],
    )]
}

fn bazel_cc_library_type_flag_group(library_type: &str, flags: &[&str]) -> BazelFlagGroup {
    let mut group = bazel_cc_flag_group(flags);
    group.expand_if_equal = Some(("libraries_to_link.type".to_owned(), library_type.to_owned()));
    group
}

fn bazel_cc_legacy_libraries_to_link_flag_sets(platform: &str) -> Vec<BazelFlagSet> {
    let mut groups = Vec::new();
    let mut start_lib = bazel_cc_library_type_flag_group("object_file_group", &["-Wl,--start-lib"]);
    start_lib.expand_if_false = Some("libraries_to_link.is_whole_archive".to_owned());
    groups.push(start_lib);

    if platform == "mac" {
        let mut object_file_group = bazel_cc_library_type_flag_group("object_file_group", &[]);
        object_file_group.iterate_over = Some("libraries_to_link.object_files".to_owned());
        object_file_group.flag_groups = vec![
            {
                let mut group = bazel_cc_flag_group(&["%{libraries_to_link.object_files}"]);
                group.expand_if_false = Some("libraries_to_link.is_whole_archive".to_owned());
                group
            },
            {
                let mut group =
                    bazel_cc_flag_group(&["-Wl,-force_load,%{libraries_to_link.object_files}"]);
                group.expand_if_true = Some("libraries_to_link.is_whole_archive".to_owned());
                group
            },
        ];
        groups.push(object_file_group);

        for library_type in ["object_file", "interface_library", "static_library"] {
            let mut group = bazel_cc_library_type_flag_group(library_type, &[]);
            group.flag_groups = vec![
                {
                    let mut group = bazel_cc_flag_group(&["%{libraries_to_link.name}"]);
                    group.expand_if_false = Some("libraries_to_link.is_whole_archive".to_owned());
                    group
                },
                {
                    let mut group =
                        bazel_cc_flag_group(&["-Wl,-force_load,%{libraries_to_link.name}"]);
                    group.expand_if_true = Some("libraries_to_link.is_whole_archive".to_owned());
                    group
                },
            ];
            groups.push(group);
        }
        groups.push(bazel_cc_library_type_flag_group(
            "dynamic_library",
            &["-l%{libraries_to_link.name}"],
        ));
        groups.push(bazel_cc_library_type_flag_group(
            "versioned_dynamic_library",
            &["%{libraries_to_link.path}"],
        ));
    } else {
        let mut whole_archive =
            bazel_cc_library_type_flag_group("static_library", &["-Wl,-whole-archive"]);
        whole_archive.expand_if_true = Some("libraries_to_link.is_whole_archive".to_owned());
        groups.push(whole_archive);
        let mut object_file_group = bazel_cc_library_type_flag_group(
            "object_file_group",
            &["%{libraries_to_link.object_files}"],
        );
        object_file_group.iterate_over = Some("libraries_to_link.object_files".to_owned());
        groups.push(object_file_group);
        for library_type in ["object_file", "interface_library", "static_library"] {
            groups.push(bazel_cc_library_type_flag_group(
                library_type,
                &["%{libraries_to_link.name}"],
            ));
        }
        groups.push(bazel_cc_library_type_flag_group(
            "dynamic_library",
            &["-l%{libraries_to_link.name}"],
        ));
        groups.push(bazel_cc_library_type_flag_group(
            "versioned_dynamic_library",
            &["-l:%{libraries_to_link.name}"],
        ));
        let mut no_whole_archive =
            bazel_cc_library_type_flag_group("static_library", &["-Wl,-no-whole-archive"]);
        no_whole_archive.expand_if_true = Some("libraries_to_link.is_whole_archive".to_owned());
        groups.push(no_whole_archive);
    }

    let mut end_lib = bazel_cc_library_type_flag_group("object_file_group", &["-Wl,--end-lib"]);
    end_lib.expand_if_false = Some("libraries_to_link.is_whole_archive".to_owned());
    groups.push(end_lib);

    let mut libraries = bazel_cc_nested_flag_group(groups);
    libraries.iterate_over = Some("libraries_to_link".to_owned());
    libraries.expand_if_available = Some("libraries_to_link".to_owned());

    vec![bazel_cc_feature_flag_set(
        "libraries_to_link",
        bazel_cc_legacy_link_actions(),
        vec![libraries],
    )]
}

fn bazel_cc_add_legacy_feature(
    selectables: &mut Vec<BazelSelectable>,
    flag_sets: &mut Vec<BazelFlagSet>,
    existing_feature_names: &[String],
    name: &str,
    feature_flag_sets: Vec<BazelFlagSet>,
) {
    if existing_feature_names
        .iter()
        .any(|existing| existing == name)
    {
        return;
    }
    selectables.push(BazelSelectable {
        name: name.to_owned(),
        requires: Vec::new(),
        implies: Vec::new(),
        provides: Vec::new(),
    });
    flag_sets.extend(feature_flag_sets);
}

fn bazel_cc_add_legacy_features(
    selectables: &mut Vec<BazelSelectable>,
    flag_sets: &mut Vec<BazelFlagSet>,
    existing_feature_names: &[String],
    platform: &str,
) {
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "shared_flag",
        bazel_cc_legacy_shared_flag_sets(),
    );
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "output_execpath_flags",
        bazel_cc_legacy_output_execpath_flag_sets(),
    );
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "runtime_library_search_directories",
        bazel_cc_legacy_runtime_library_search_directories_flag_sets(platform),
    );
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "library_search_directories",
        bazel_cc_legacy_library_search_directories_flag_sets(),
    );
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "archiver_flags",
        bazel_cc_legacy_archiver_flag_sets(platform),
    );
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "libraries_to_link",
        bazel_cc_legacy_libraries_to_link_flag_sets(platform),
    );
    bazel_cc_add_legacy_feature(
        selectables,
        flag_sets,
        existing_feature_names,
        "user_link_flags",
        bazel_cc_legacy_user_link_flags_flag_sets(),
    );
}

fn bazel_cc_parse_toolchain_features<'v>(
    toolchain_config_info: Value<'v>,
    tools_directory: String,
    heap: Heap<'v>,
) -> starlark::Result<BazelCcToolchainFeatures> {
    let mut selectables = Vec::new();
    let mut default_selectables = Vec::new();
    let mut action_tools = Vec::new();
    let mut flag_sets = Vec::new();
    let mut env_sets = Vec::new();
    let mut feature_names = Vec::new();
    let mut action_config_names = Vec::new();

    if let Some(features) = bazel_cc_attr(toolchain_config_info, "_features_DO_NOT_USE", heap)? {
        for feature in bazel_cc_sequence_values(features, "_features_DO_NOT_USE")? {
            let Some(name) = bazel_cc_string_attr(feature, "name", heap)? else {
                continue;
            };
            feature_names.push(name.clone());
            let enabled = bazel_cc_bool_attr(feature, "enabled", heap)?;
            if enabled {
                bazel_cc_push_unique(&mut default_selectables, name.clone());
            }
            if let Some(feature_flag_sets) = bazel_cc_attr(feature, "flag_sets", heap)? {
                for flag_set in bazel_cc_sequence_values(feature_flag_sets, "flag_sets")? {
                    flag_sets.push(bazel_cc_parse_flag_set(&name, false, None, flag_set, heap)?);
                }
            }
            if let Some(feature_env_sets) = bazel_cc_attr(feature, "env_sets", heap)? {
                for env_set in bazel_cc_sequence_values(feature_env_sets, "env_sets")? {
                    env_sets.push(bazel_cc_parse_env_set(&name, env_set, heap)?);
                }
            }
            selectables.push(BazelSelectable {
                name,
                requires: bazel_cc_parse_requires(feature, heap)?,
                implies: bazel_cc_string_sequence_attr(feature, "implies", heap)?,
                provides: bazel_cc_string_sequence_attr(feature, "provides", heap)?,
            });
        }
    }

    if let Some(action_configs) =
        bazel_cc_attr(toolchain_config_info, "_action_configs_DO_NOT_USE", heap)?
    {
        for action_config in bazel_cc_sequence_values(action_configs, "_action_configs_DO_NOT_USE")?
        {
            let Some(action_name) = bazel_cc_string_attr(action_config, "action_name", heap)?
            else {
                continue;
            };
            action_config_names.push(action_name.clone());
            let enabled = bazel_cc_bool_attr(action_config, "enabled", heap)?;
            if enabled {
                bazel_cc_push_unique(&mut default_selectables, action_name.clone());
            }
            selectables.push(BazelSelectable {
                name: action_name.clone(),
                requires: Vec::new(),
                implies: bazel_cc_string_sequence_attr(action_config, "implies", heap)?,
                provides: Vec::new(),
            });

            if let Some(tools) = bazel_cc_attr(action_config, "tools", heap)? {
                for tool in bazel_cc_sequence_values(tools, "tools")? {
                    action_tools.push(bazel_cc_parse_tool(&action_name, tool, heap)?);
                }
            }
            if let Some(action_flag_sets) = bazel_cc_attr(action_config, "flag_sets", heap)? {
                for flag_set in bazel_cc_sequence_values(action_flag_sets, "flag_sets")? {
                    flag_sets.push(bazel_cc_parse_flag_set(
                        &action_name,
                        true,
                        Some(&action_name),
                        flag_set,
                        heap,
                    )?);
                }
            }
        }
    }

    let add_legacy_features = !feature_names
        .iter()
        .any(|name| name == "no_legacy_features");
    let platform = if bazel_cc_string_attr(toolchain_config_info, "target_libc", heap)?.as_deref()
        == Some("macosx")
    {
        "mac"
    } else {
        "linux"
    };
    if add_legacy_features {
        bazel_cc_add_legacy_features(&mut selectables, &mut flag_sets, &feature_names, platform);
    }

    if add_legacy_features {
        let tool_paths = bazel_cc_parse_tool_paths(toolchain_config_info, heap)?;
        bazel_cc_add_legacy_action_configs(
            &mut selectables,
            &mut action_tools,
            &tool_paths,
            &action_config_names,
            add_legacy_features,
        );
    }

    let artifact_name_patterns =
        bazel_cc_parse_artifact_name_patterns(toolchain_config_info, heap)?;

    bazel_cc_validate_selectables(&selectables)?;

    Ok(BazelCcToolchainFeatures {
        selectables,
        default_selectables,
        action_tools: Arc::new(action_tools),
        flag_sets: Arc::new(flag_sets),
        env_sets: Arc::new(env_sets),
        artifact_name_patterns,
        tools_directory,
        feature_configuration_cache: Mutex::new(HashMap::new()),
    })
}

fn bazel_cc_enabled_selectables(
    selectables: &[BazelSelectable],
    requested_features: &[String],
) -> starlark::Result<Vec<String>> {
    let mut enabled = Vec::new();
    let mut requested = Vec::new();
    for requested_feature in requested_features {
        if let Some(index) = bazel_cc_selectable_index(selectables, requested_feature) {
            bazel_cc_push_unique_index(&mut requested, index);
            bazel_cc_enable_all_implied_by(selectables, &mut enabled, index);
        }
    }

    loop {
        let mut changed = false;
        for index in 0..selectables.len() {
            if !enabled.contains(&index) {
                continue;
            }
            if !bazel_cc_is_selectable_satisfied(selectables, &enabled, &requested, index) {
                enabled.retain(|enabled_index| *enabled_index != index);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    bazel_cc_check_provides_conflicts(selectables, &enabled)?;

    Ok(selectables
        .iter()
        .enumerate()
        .filter_map(|(index, selectable)| enabled.contains(&index).then(|| selectable.name.clone()))
        .collect())
}

fn bazel_cc_selectable_index(selectables: &[BazelSelectable], name: &str) -> Option<usize> {
    selectables
        .iter()
        .position(|selectable| selectable.name == name)
}

fn bazel_cc_push_unique_index(values: &mut Vec<usize>, value: usize) -> bool {
    if values.contains(&value) {
        false
    } else {
        values.push(value);
        true
    }
}

fn bazel_cc_enable_all_implied_by(
    selectables: &[BazelSelectable],
    enabled: &mut Vec<usize>,
    index: usize,
) {
    if !bazel_cc_push_unique_index(enabled, index) {
        return;
    }
    for implied in &selectables[index].implies {
        if let Some(implied_index) = bazel_cc_selectable_index(selectables, implied) {
            bazel_cc_enable_all_implied_by(selectables, enabled, implied_index);
        }
    }
}

fn bazel_cc_is_selectable_satisfied(
    selectables: &[BazelSelectable],
    enabled: &[usize],
    requested: &[usize],
    index: usize,
) -> bool {
    (requested.contains(&index)
        || selectables
            .iter()
            .enumerate()
            .any(|(other_index, selectable)| {
                enabled.contains(&other_index)
                    && selectable
                        .implies
                        .iter()
                        .any(|implied| implied == &selectables[index].name)
            }))
        && selectables[index].implies.iter().all(|implied| {
            bazel_cc_selectable_index(selectables, implied)
                .is_some_and(|implied_index| enabled.contains(&implied_index))
        })
        && (selectables[index].requires.is_empty()
            || selectables[index].requires.iter().any(|required_set| {
                required_set.iter().all(|required| {
                    bazel_cc_selectable_index(selectables, required)
                        .is_some_and(|required_index| enabled.contains(&required_index))
                })
            }))
}

fn bazel_cc_validate_selectables(selectables: &[BazelSelectable]) -> starlark::Result<()> {
    for (index, selectable) in selectables.iter().enumerate() {
        if selectables[..index]
            .iter()
            .any(|existing| existing.name == selectable.name)
        {
            return Err(bazel_cc_error(format!(
                "Invalid toolchain configuration: feature or action config '{}' was specified multiple times.",
                selectable.name
            )));
        }
        for implied in &selectable.implies {
            if bazel_cc_selectable_index(selectables, implied).is_none() {
                return Err(bazel_cc_error(format!(
                    "Invalid toolchain configuration: '{}' implies unknown feature or action config '{}'.",
                    selectable.name, implied
                )));
            }
        }
        for required_set in &selectable.requires {
            for required in required_set {
                if bazel_cc_selectable_index(selectables, required).is_none() {
                    return Err(bazel_cc_error(format!(
                        "Invalid toolchain configuration: '{}' requires unknown feature or action config '{}'.",
                        selectable.name, required
                    )));
                }
            }
        }
    }
    Ok(())
}

fn bazel_cc_check_provides_conflicts(
    selectables: &[BazelSelectable],
    enabled: &[usize],
) -> starlark::Result<()> {
    let mut provided = Vec::<(&str, &str)>::new();
    for index in enabled {
        let selectable = &selectables[*index];
        for provides in &selectable.provides {
            if let Some((_, existing)) = provided
                .iter()
                .find(|(provided_name, _)| *provided_name == provides.as_str())
            {
                return Err(bazel_cc_error(format!(
                    "Invalid toolchain configuration: features '{}' and '{}' both provide '{}'.",
                    existing, selectable.name, provides
                )));
            }
            provided.push((provides, selectable.name.as_str()));
        }
    }
    Ok(())
}

impl BazelCcToolchainFeatures {
    fn configure_features(
        &self,
        requested_features: Vec<String>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        if let Some(data) = self
            .feature_configuration_cache
            .lock()
            .map_err(|_| {
                bazel_cc_error("CcToolchainFeatures feature configuration cache lock was poisoned")
            })?
            .get(&requested_features)
            .cloned()
        {
            return Ok(BazelFeatureConfiguration {
                requested_features,
                data,
            });
        }

        let data = Arc::new(self.compute_feature_configuration_data(&requested_features)?);
        let mut cache = self.feature_configuration_cache.lock().map_err(|_| {
            bazel_cc_error("CcToolchainFeatures feature configuration cache lock was poisoned")
        })?;
        let data = cache
            .entry(requested_features.clone())
            .or_insert_with(|| data.clone())
            .clone();

        Ok(BazelFeatureConfiguration {
            requested_features,
            data,
        })
    }

    fn compute_feature_configuration_data(
        &self,
        requested_features: &[String],
    ) -> starlark::Result<BazelFeatureConfigurationData> {
        let enabled_selectables =
            bazel_cc_enabled_selectables(&self.selectables, requested_features)?;
        let enabled_selectable_set = enabled_selectables.iter().cloned().collect::<HashSet<_>>();

        let mut action_config_flag_sets = Vec::new();
        let mut feature_flag_sets = Vec::new();
        let mut action_config_flag_sets_by_action = HashMap::<String, Vec<usize>>::new();
        let mut feature_flag_sets_by_action = HashMap::<String, Vec<usize>>::new();
        for flag_set in self.flag_sets.iter() {
            if !bazel_cc_flag_set_enabled(&enabled_selectable_set, flag_set) {
                continue;
            }
            if flag_set.owner_is_action_config {
                let index = action_config_flag_sets.len();
                for action in &flag_set.actions {
                    action_config_flag_sets_by_action
                        .entry(action.clone())
                        .or_default()
                        .push(index);
                }
                action_config_flag_sets.push(flag_set.clone());
            } else {
                let index = feature_flag_sets.len();
                for action in &flag_set.actions {
                    feature_flag_sets_by_action
                        .entry(action.clone())
                        .or_default()
                        .push(index);
                }
                feature_flag_sets.push(flag_set.clone());
            }
        }

        let mut env_sets = Vec::new();
        let mut env_sets_by_action = HashMap::<String, Vec<usize>>::new();
        for env_set in self.env_sets.iter() {
            if !bazel_cc_env_set_enabled(&enabled_selectable_set, env_set) {
                continue;
            }
            let index = env_sets.len();
            for action in &env_set.actions {
                env_sets_by_action
                    .entry(action.clone())
                    .or_default()
                    .push(index);
            }
            env_sets.push(env_set.clone());
        }

        let mut selected_action_tools = HashMap::new();
        for tool in self.action_tools.iter() {
            if tool.matches(&enabled_selectable_set) {
                selected_action_tools
                    .entry(tool.action_name.clone())
                    .or_insert_with(|| tool.clone());
            }
        }

        Ok(BazelFeatureConfigurationData {
            enabled_selectable_set,
            action_tools: self.action_tools.clone(),
            action_config_flag_sets,
            feature_flag_sets,
            env_sets,
            selected_action_tools,
            action_config_flag_sets_by_action,
            feature_flag_sets_by_action,
            env_sets_by_action,
            tools_directory: self.tools_directory.clone(),
        })
    }
}

impl BazelWithFeatureSet {
    fn matches(&self, enabled: &HashSet<String>) -> bool {
        self.features
            .iter()
            .all(|feature| enabled.contains(feature))
            && self
                .not_features
                .iter()
                .all(|feature| !enabled.contains(feature))
    }
}

impl BazelActionTool {
    fn matches(&self, enabled: &HashSet<String>) -> bool {
        self.with_features.is_empty()
            || self
                .with_features
                .iter()
                .any(|with_features| with_features.matches(enabled))
    }

    fn tool_path(&self, tools_directory: &str) -> String {
        match self.path_origin {
            BazelToolPathOrigin::FilesystemRoot | BazelToolPathOrigin::WorkspaceRoot => {
                self.path.clone()
            }
            BazelToolPathOrigin::CrosstoolPackage => {
                if tools_directory.is_empty() || self.path.starts_with('/') {
                    self.path.clone()
                } else {
                    format!(
                        "{}/{}",
                        tools_directory.trim_end_matches('/'),
                        self.path.trim_start_matches('/')
                    )
                }
            }
        }
    }
}

fn bazel_cc_toolchain_features_from_toolchain<'v>(
    cc_toolchain: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<&'v BazelCcToolchainFeatures> {
    let Some(toolchain_features) = bazel_cc_attr(cc_toolchain, "_toolchain_features", heap)? else {
        return Err(bazel_cc_error(
            "Expected cc_toolchain to expose a `_toolchain_features` attribute",
        ));
    };
    toolchain_features
        .downcast_ref::<BazelCcToolchainFeatures>()
        .ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected cc_toolchain._toolchain_features to be CcToolchainFeatures, got `{}`",
                toolchain_features.get_type()
            ))
        })
}

fn bazel_cc_artifact_name_pattern<'a>(
    features: &'a BazelCcToolchainFeatures,
    category: &'static BazelArtifactCategory,
) -> (&'a str, &'a str) {
    features
        .artifact_name_patterns
        .iter()
        .find(|pattern| pattern.category == category.name)
        .map(|pattern| (pattern.prefix.as_str(), pattern.extension.as_str()))
        .unwrap_or((category.default_prefix, category.default_extension))
}

fn bazel_cc_artifact_name(output_name: &str, prefix: &str, extension: &str) -> String {
    let artifact_basename = match output_name.rsplit_once('/') {
        Some((parent, basename)) => {
            return format!("{parent}/{prefix}{basename}{extension}");
        }
        None => output_name,
    };
    format!("{prefix}{artifact_basename}{extension}")
}

impl BazelFeatureConfiguration {
    fn is_enabled_selectable(&self, name: &str) -> bool {
        self.data.enabled_selectable_set.contains(name)
    }

    fn action_is_configured(&self, action_name: &str) -> bool {
        self.data
            .action_tools
            .iter()
            .any(|tool| tool.action_name == action_name)
    }

    fn selected_tool(&self, action_name: &str) -> starlark::Result<&BazelActionTool> {
        if let Some(tool) = self.data.selected_action_tools.get(action_name) {
            return Ok(tool);
        }

        let candidate_count = self
            .data
            .action_tools
            .iter()
            .filter(|tool| tool.action_name == action_name)
            .count();
        let known_actions = self
            .data
            .action_tools
            .iter()
            .map(|tool| tool.action_name.as_str())
            .take(20)
            .collect::<Vec<_>>()
            .join(", ");
        Err(bazel_cc_error(format!(
            "Matching tool for action {action_name} not found for given feature configuration; candidate tools: {candidate_count}; known action tools: [{known_actions}]"
        )))
    }

    fn action_config_flag_sets_for<'a>(
        &'a self,
        action_name: &'a str,
    ) -> impl Iterator<Item = &'a BazelFlagSet> + 'a {
        self.data
            .action_config_flag_sets_by_action
            .get(action_name)
            .into_iter()
            .flatten()
            .map(|index| &self.data.action_config_flag_sets[*index])
    }

    fn feature_flag_sets_for<'a>(
        &'a self,
        action_name: &'a str,
    ) -> impl Iterator<Item = &'a BazelFlagSet> + 'a {
        self.data
            .feature_flag_sets_by_action
            .get(action_name)
            .into_iter()
            .flatten()
            .map(|index| &self.data.feature_flag_sets[*index])
    }

    fn env_sets_for<'a>(
        &'a self,
        action_name: &'a str,
    ) -> impl Iterator<Item = &'a BazelEnvSet> + 'a {
        self.data
            .env_sets_by_action
            .get(action_name)
            .into_iter()
            .flatten()
            .map(|index| &self.data.env_sets[*index])
    }
}

fn bazel_cc_non_none_attr<'v>(
    value: Value<'v>,
    attr: &str,
    heap: Heap<'v>,
) -> starlark::Result<Option<Value<'v>>> {
    Ok(bazel_cc_attr(value, attr, heap)?.filter(|value| !value.is_none()))
}

fn bazel_cc_nested_attr<'v>(
    value: Value<'v>,
    attrs: &[&str],
    heap: Heap<'v>,
) -> starlark::Result<Option<Value<'v>>> {
    let mut value = value;
    for attr in attrs {
        let Some(next) = bazel_cc_attr(value, attr, heap)? else {
            return Ok(None);
        };
        value = next;
    }
    Ok(Some(value))
}

fn bazel_cc_add_requested_feature(
    requested: &mut HashSet<String>,
    unsupported: &HashSet<String>,
    feature: &str,
) {
    if !unsupported.contains(feature) {
        requested.insert(feature.to_owned());
    }
}

fn bazel_cc_add_requested_features<'a>(
    requested: &mut HashSet<String>,
    unsupported: &HashSet<String>,
    features: impl IntoIterator<Item = &'a str>,
) {
    for feature in features {
        bazel_cc_add_requested_feature(requested, unsupported, feature);
    }
}

fn bazel_cc_language(
    language: Value<'_>,
    requested_features: &[String],
) -> starlark::Result<String> {
    let mut language = if language.is_none() {
        "c++"
    } else {
        language.unpack_str().ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected C++ language to be a string or None, got `{}`",
                language.get_type()
            ))
        })?
    }
    .replace('+', "p");
    if requested_features
        .iter()
        .any(|feature| feature == "lang_objc")
    {
        language = "objc".to_owned();
    }
    Ok(language)
}

fn bazel_cc_configure_features_from_ctx<'v>(
    ctx: Value<'v>,
    cc_toolchain: Value<'v>,
    language: Value<'v>,
    requested_features: Vec<String>,
    unsupported_features: Vec<String>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<BazelFeatureConfiguration> {
    let heap = eval.heap();
    let language = bazel_cc_language(language, &requested_features)?;
    let cpp_configuration = bazel_cc_nested_attr(ctx, &["fragments", "cpp"], heap)?
        .ok_or_else(|| bazel_cc_error("cpp configuration fragment is missing"))?;

    let mut all_requested_features = HashSet::<String>::new();
    let mut all_unsupported_features = unsupported_features.into_iter().collect::<HashSet<_>>();

    if !bazel_cc_bool_attr(cc_toolchain, "_supports_header_parsing", heap)? {
        all_unsupported_features.insert("parse_headers".to_owned());
    }

    let module_map = bazel_cc_nested_attr(
        cc_toolchain,
        &["_cc_info", "compilation_context", "_module_map"],
        heap,
    )?;
    if language != "objc" && language != "objcpp" && module_map.is_none_or(|value| value.is_none())
    {
        all_unsupported_features.insert("module_maps".to_owned());
    }

    all_requested_features.insert("no_generate_debug_symbols".to_owned());
    if bazel_cc_bool_attr(cpp_configuration, "apple_generate_dsym", heap)? {
        all_requested_features.remove("no_generate_debug_symbols");
        all_requested_features.insert("generate_dsym_file".to_owned());
    }

    if language == "objc" || language == "objcpp" {
        all_requested_features.insert("lang_objc".to_owned());
        if bazel_cc_bool_attr(cpp_configuration, "objc_generate_linkmap", heap)? {
            all_requested_features.insert("generate_linkmap".to_owned());
        }
        if bazel_cc_bool_attr(cpp_configuration, "objc_should_strip_binary", heap)? {
            all_requested_features.insert("dead_strip".to_owned());
        }
    }

    let features = bazel_cc_toolchain_features_from_toolchain(cc_toolchain, heap)?;
    bazel_cc_add_requested_feature(
        &mut all_requested_features,
        &all_unsupported_features,
        "fastbuild",
    );
    bazel_cc_add_requested_features(
        &mut all_requested_features,
        &all_unsupported_features,
        BAZEL_CC_ALL_COMPILE_ACTIONS.iter().copied(),
    );
    bazel_cc_add_requested_features(
        &mut all_requested_features,
        &all_unsupported_features,
        BAZEL_CC_ALL_LINK_ACTIONS.iter().copied(),
    );
    bazel_cc_add_requested_features(
        &mut all_requested_features,
        &all_unsupported_features,
        BAZEL_CC_ALL_ARCHIVE_ACTIONS.iter().copied(),
    );
    bazel_cc_add_requested_features(
        &mut all_requested_features,
        &all_unsupported_features,
        BAZEL_CC_ALL_OTHER_ACTIONS.iter().copied(),
    );
    bazel_cc_add_requested_features(
        &mut all_requested_features,
        &all_unsupported_features,
        requested_features.iter().map(String::as_str),
    );
    bazel_cc_add_requested_features(
        &mut all_requested_features,
        &all_unsupported_features,
        features.default_selectables.iter().map(String::as_str),
    );

    if language == "objc" || language == "objcpp" {
        bazel_cc_add_requested_features(
            &mut all_requested_features,
            &all_unsupported_features,
            BAZEL_CC_OBJC_ACTIONS.iter().copied(),
        );
    }

    if !bazel_cc_bool_attr(cpp_configuration, "_dont_enable_host_nonhost", heap)? {
        let host_or_nonhost = if bazel_cc_bool_attr(cc_toolchain, "_is_tool_configuration", heap)? {
            "host"
        } else {
            "nonhost"
        };
        bazel_cc_add_requested_feature(
            &mut all_requested_features,
            &all_unsupported_features,
            host_or_nonhost,
        );
    }

    let coverage_enabled = bazel_cc_nested_attr(ctx, &["configuration", "coverage_enabled"], heap)?
        .and_then(|value| value.unpack_bool())
        .unwrap_or(false);
    if coverage_enabled {
        all_requested_features.insert("coverage".to_owned());
        all_requested_features.insert("gcc_coverage_map_format".to_owned());
    }

    let fdo_context = bazel_cc_non_none_attr(cc_toolchain, "_fdo_context", heap)?;
    let branch_fdo_provider = match fdo_context {
        Some(fdo_context) => bazel_cc_non_none_attr(fdo_context, "branch_fdo_profile", heap)?,
        None => None,
    };
    let propeller_optimize_info = match fdo_context {
        Some(fdo_context) => bazel_cc_non_none_attr(fdo_context, "propeller_optimize_info", heap)?,
        None => None,
    };
    let enable_propeller_optimize = match propeller_optimize_info {
        Some(propeller) => {
            bazel_cc_non_none_attr(propeller, "cc_profile", heap)?.is_some()
                || bazel_cc_non_none_attr(propeller, "ld_profile", heap)?.is_some()
        }
        None => false,
    };

    let compilation_mode = "fastbuild";
    if let Some(branch_fdo_provider) = branch_fdo_provider
        && compilation_mode == "opt"
    {
        let branch_fdo_mode =
            bazel_cc_string_attr(branch_fdo_provider, "branch_fdo_mode", heap)?.unwrap_or_default();
        if branch_fdo_mode == "llvm_fdo" || branch_fdo_mode == "llvm_cs_fdo" {
            bazel_cc_add_requested_feature(
                &mut all_requested_features,
                &all_unsupported_features,
                "fdo_optimize",
            );
            bazel_cc_add_requested_feature(
                &mut all_requested_features,
                &all_unsupported_features,
                "enable_fdo_memprof_optimize",
            );
            bazel_cc_add_requested_feature(
                &mut all_requested_features,
                &all_unsupported_features,
                "enable_fdo_thinlto",
            );
            if !enable_propeller_optimize {
                bazel_cc_add_requested_feature(
                    &mut all_requested_features,
                    &all_unsupported_features,
                    "enable_fdo_split_functions",
                );
            }
        }
        if branch_fdo_mode == "llvm_cs_fdo" {
            all_requested_features.insert("cs_fdo_optimize".to_owned());
        }
        if branch_fdo_mode == "auto_fdo" {
            all_requested_features.insert("autofdo".to_owned());
            bazel_cc_add_requested_feature(
                &mut all_requested_features,
                &all_unsupported_features,
                "enable_autofdo_memprof_optimize",
            );
            bazel_cc_add_requested_feature(
                &mut all_requested_features,
                &all_unsupported_features,
                "enable_afdo_thinlto",
            );
            if !all_unsupported_features.contains("fsafdo") {
                all_requested_features.insert("enable_fsafdo".to_owned());
                bazel_cc_add_requested_feature(
                    &mut all_requested_features,
                    &all_unsupported_features,
                    "enable_fdo_split_functions",
                );
            }
        }
        if branch_fdo_mode == "xbinary_fdo" {
            all_requested_features.insert("xbinaryfdo".to_owned());
            bazel_cc_add_requested_feature(
                &mut all_requested_features,
                &all_unsupported_features,
                "enable_xbinaryfdo_thinlto",
            );
        }
    }

    if bazel_cc_non_none_attr(cpp_configuration, "_fdo_prefetch_hints_label", heap)?.is_some() {
        all_requested_features.insert("fdo_prefetch_hints".to_owned());
    }
    if enable_propeller_optimize {
        all_requested_features.insert("propeller_optimize".to_owned());
    }

    let mut requested_features = all_requested_features.into_iter().collect::<Vec<_>>();
    requested_features.sort();
    requested_features.dedup();
    let feature_configuration = features.configure_features(requested_features)?;

    for unsupported in all_unsupported_features {
        if feature_configuration.is_enabled_selectable(&unsupported) {
            let label = bazel_cc_attr(cc_toolchain, "_toolchain_label", heap)?
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unknown>".to_owned());
            return Err(bazel_cc_error(format!(
                "The C++ toolchain '{label}' unconditionally implies feature '{unsupported}', which is unsupported by this rule. This is most likely a misconfiguration in the C++ toolchain."
            )));
        }
    }

    Ok(feature_configuration)
}

fn bazel_cc_flag_set_enabled(
    enabled_selectable_set: &HashSet<String>,
    flag_set: &BazelFlagSet,
) -> bool {
    enabled_selectable_set.contains(&flag_set.owner_selectable)
        && (flag_set.with_features.is_empty()
            || flag_set
                .with_features
                .iter()
                .any(|with_features| with_features.matches(enabled_selectable_set)))
}

fn bazel_cc_env_set_enabled(
    enabled_selectable_set: &HashSet<String>,
    env_set: &BazelEnvSet,
) -> bool {
    enabled_selectable_set.contains(&env_set.owner_selectable)
        && (env_set.with_features.is_empty()
            || env_set
                .with_features
                .iter()
                .any(|with_features| with_features.matches(enabled_selectable_set)))
}

fn bazel_cc_feature_variable<'v>(
    variables: Value<'v>,
    locals: &[(String, Value<'v>)],
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Option<Value<'v>>> {
    if let Some((_, value)) = locals
        .iter()
        .rev()
        .find(|(local_name, _)| local_name == name)
    {
        return Ok(Some(*value));
    }

    let Some((root, rest)) = name.split_once('.') else {
        return Ok(bazel_cc_build_variable(variables, name));
    };

    let mut value = if let Some((_, value)) = locals
        .iter()
        .rev()
        .find(|(local_name, _)| local_name == root)
    {
        *value
    } else {
        let Some(value) = bazel_cc_build_variable(variables, root) else {
            return Ok(None);
        };
        value
    };

    for field in rest.split('.') {
        if let Some(attr) = value.get_attr(field, heap)? {
            value = attr;
            continue;
        }
        if let Some(dict) = DictRef::from_value(value)
            && let Some((_, dict_value)) =
                dict.iter().find(|(key, _)| key.unpack_str() == Some(field))
        {
            value = dict_value;
            continue;
        }
        return Ok(None);
    }

    Ok(Some(value))
}

fn bazel_cc_feature_variable_available<'v>(
    variables: Value<'v>,
    locals: &[(String, Value<'v>)],
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<bool> {
    Ok(bazel_cc_feature_variable(variables, locals, name, heap)?
        .is_some_and(|value| !value.is_none()))
}

fn bazel_cc_feature_string<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<String> {
    if let Some(value) = value.unpack_str() {
        return Ok(value.to_owned());
    }
    if let Some(value) = value.unpack_bool() {
        return Ok(if value { "1" } else { "0" }.to_owned());
    }
    if let Some(value) = value.unpack_i32() {
        return Ok(value.to_string());
    }
    bazel_cc_link_string(value, heap)
}

fn bazel_cc_expand_feature_flag<'v>(
    flag: &str,
    variables: Value<'v>,
    locals: &[(String, Value<'v>)],
    heap: Heap<'v>,
) -> starlark::Result<String> {
    let Some(mut start) = flag.find("%{") else {
        return Ok(flag.to_owned());
    };
    let mut expanded = String::with_capacity(flag.len());
    let mut rest = flag;
    loop {
        expanded.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            return Err(bazel_cc_error(format!(
                "Unterminated C++ toolchain variable in flag `{flag}`"
            )));
        };
        let variable_name = &after_start[..end];
        let value = bazel_cc_feature_variable(variables, locals, variable_name, heap)?.ok_or_else(
            || {
                bazel_cc_error(format!(
                    "C++ toolchain flag `{flag}` references unavailable variable `{variable_name}`"
                ))
            },
        )?;
        expanded.push_str(&bazel_cc_feature_string(value, heap)?);
        rest = &after_start[end + 1..];
        let Some(next_start) = rest.find("%{") else {
            expanded.push_str(rest);
            return Ok(expanded);
        };
        start = next_start;
    }
}

fn bazel_cc_flag_group_conditions_match<'v>(
    flag_group: &BazelFlagGroup,
    variables: Value<'v>,
    locals: &[(String, Value<'v>)],
    heap: Heap<'v>,
) -> starlark::Result<bool> {
    if let Some(variable) = &flag_group.expand_if_available
        && !bazel_cc_feature_variable_available(variables, locals, variable, heap)?
    {
        return Ok(false);
    }
    if let Some(variable) = &flag_group.expand_if_not_available
        && bazel_cc_feature_variable_available(variables, locals, variable, heap)?
    {
        return Ok(false);
    }
    if let Some(variable) = &flag_group.expand_if_true {
        let Some(value) = bazel_cc_feature_variable(variables, locals, variable, heap)? else {
            return Ok(false);
        };
        if !value.to_bool() {
            return Ok(false);
        }
    }
    if let Some(variable) = &flag_group.expand_if_false {
        let Some(value) = bazel_cc_feature_variable(variables, locals, variable, heap)? else {
            return Ok(false);
        };
        if value.to_bool() {
            return Ok(false);
        }
    }
    if let Some((variable, expected)) = &flag_group.expand_if_equal {
        let Some(value) = bazel_cc_feature_variable(variables, locals, variable, heap)? else {
            return Ok(false);
        };
        if bazel_cc_feature_string(value, heap)? != *expected {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bazel_cc_expand_feature_flag_group_visit<'v>(
    flag_group: &BazelFlagGroup,
    variables: Value<'v>,
    locals: &mut Vec<(String, Value<'v>)>,
    heap: Heap<'v>,
    visit: &mut impl FnMut(String) -> starlark::Result<()>,
) -> starlark::Result<()> {
    if !bazel_cc_flag_group_conditions_match(flag_group, variables, locals, heap)? {
        return Ok(());
    }

    if let Some(iterate_over) = &flag_group.iterate_over {
        let value =
            bazel_cc_feature_variable(variables, locals, iterate_over, heap)?.ok_or_else(|| {
                bazel_cc_error(format!(
                    "C++ toolchain flag_group iterates over unavailable variable `{iterate_over}`"
                ))
            })?;
        for item in bazel_cc_link_sequence_values(value, iterate_over)? {
            locals.push((iterate_over.clone(), item));
            for nested in &flag_group.flag_groups {
                bazel_cc_expand_feature_flag_group_visit(nested, variables, locals, heap, visit)?;
            }
            for flag in &flag_group.flags {
                let flag = bazel_cc_expand_feature_flag(flag, variables, locals, heap)?;
                visit(flag)?;
            }
            locals.pop();
        }
    } else {
        for nested in &flag_group.flag_groups {
            bazel_cc_expand_feature_flag_group_visit(nested, variables, locals, heap, visit)?;
        }
        for flag in &flag_group.flags {
            let flag = bazel_cc_expand_feature_flag(flag, variables, locals, heap)?;
            visit(flag)?;
        }
    }
    Ok(())
}

fn bazel_cc_feature_command_line_visit<'v>(
    feature_configuration: &BazelFeatureConfiguration,
    action_name: &str,
    variables: Value<'v>,
    heap: Heap<'v>,
    visit: &mut impl FnMut(String) -> starlark::Result<()>,
) -> starlark::Result<()> {
    let mut locals = Vec::new();

    for flag_set in feature_configuration
        .action_config_flag_sets_for(action_name)
        .chain(feature_configuration.feature_flag_sets_for(action_name))
    {
        for flag_group in &flag_set.flag_groups {
            bazel_cc_expand_feature_flag_group_visit(
                flag_group,
                variables,
                &mut locals,
                heap,
                visit,
            )?;
        }
    }

    Ok(())
}

fn bazel_cc_feature_command_line_strings<'v>(
    feature_configuration: &BazelFeatureConfiguration,
    action_name: &str,
    variables: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<String>> {
    let mut args = Vec::new();
    bazel_cc_feature_command_line_visit(
        feature_configuration,
        action_name,
        variables,
        heap,
        &mut |arg| {
            args.push(arg);
            Ok(())
        },
    )?;
    Ok(args)
}

fn bazel_cc_feature_command_line<'v>(
    feature_configuration: &BazelFeatureConfiguration,
    action_name: &str,
    variables: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    Ok(
        bazel_cc_feature_command_line_strings(feature_configuration, action_name, variables, heap)?
            .into_iter()
            .map(|arg| heap.alloc_str(&arg).to_value())
            .collect(),
    )
}

fn bazel_cc_link_param_file<'v>(
    args: Vec<Value<'v>>,
    variables: Value<'v>,
    parameter_file_type: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Value<'v>> {
    if parameter_file_type.is_none() {
        return Ok(heap.alloc(AllocList(args)));
    }

    let Some(parameter_file_type) = parameter_file_type.unpack_str() else {
        return Err(bazel_cc_error(format!(
            "Expected parameter_file_type to be a string or None, got `{}`",
            parameter_file_type.get_type()
        )));
    };

    let linker_param_file =
        match bazel_cc_build_variable(variables, BAZEL_LINKER_PARAM_FILE_VARIABLE) {
            Some(value) => bazel_cc_feature_string(value, heap)?,
            None => BAZEL_LINKER_PARAM_FILE_PLACEHOLDER.to_owned(),
        };

    let Some(param_file_arg) = args
        .iter()
        .filter_map(|arg| arg.unpack_str())
        .find(|arg| arg.contains(&linker_param_file))
    else {
        return Ok(heap.alloc(AllocList(args)));
    };

    let arg_format = param_file_arg.replace(&linker_param_file, "{}");
    let args = args.into_iter().filter(|arg| {
        arg.unpack_str()
            .map_or(true, |arg| !arg.contains(&linker_param_file))
    });
    Ok(
        heap.alloc(StarlarkCmdArgs::from_values_with_bazel_param_file(
            args,
            heap.alloc_str(&arg_format).to_string_value(),
            parameter_file_type,
        )?),
    )
}

fn bazel_cc_feature_environment_strings<'v>(
    feature_configuration: &BazelFeatureConfiguration,
    action_name: &str,
    variables: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<(String, String)>> {
    let locals = Vec::new();
    let mut env = SmallMap::new();

    for env_set in feature_configuration.env_sets_for(action_name) {
        for entry in &env_set.env_entries {
            if let Some(variable) = &entry.expand_if_available
                && !bazel_cc_feature_variable_available(variables, &locals, variable, heap)?
            {
                continue;
            }
            let value = bazel_cc_expand_feature_flag(&entry.value, variables, &locals, heap)?;
            env.insert(entry.key.clone(), value);
        }
    }

    Ok(env.into_iter().collect())
}

fn bazel_cc_feature_environment<'v>(
    feature_configuration: &BazelFeatureConfiguration,
    action_name: &str,
    variables: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Vec<(String, Value<'v>)>> {
    Ok(
        bazel_cc_feature_environment_strings(feature_configuration, action_name, variables, heap)?
            .into_iter()
            .map(|(key, value)| (key, heap.alloc_str(&value).to_value()))
            .collect(),
    )
}

#[starlark_value(type = "FeatureConfiguration")]
impl<'v> StarlarkValue<'v> for BazelFeatureConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_feature_configuration_methods)
    }
}

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for BazelCcInternal {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_cc_internal_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "check_private_api".to_owned(),
            "cc_toolchain_features".to_owned(),
            "cc_toolchain_variables".to_owned(),
            "combine_cc_toolchain_variables".to_owned(),
            "actions2ctx_cheat".to_owned(),
            "compute_output_name_prefix_dir".to_owned(),
            "create_cc_compile_action".to_owned(),
            "create_header_info".to_owned(),
            "create_header_info_with_deps".to_owned(),
            "declare_compile_output_file".to_owned(),
            "dynamic_library_symlink".to_owned(),
            "dynamic_library_symlink2".to_owned(),
            "dynamic_library_soname".to_owned(),
            "exec_os".to_owned(),
            "freeze".to_owned(),
            "get_artifact_name_extension_for_category".to_owned(),
            "get_artifact_name_for_category".to_owned(),
            "get_link_args".to_owned(),
            "intern_seq".to_owned(),
            "intern_string_sequence_variable_value".to_owned(),
            "is_tree_artifact".to_owned(),
            "per_file_copts".to_owned(),
            "rule_class".to_owned(),
            "solib_symlink_action".to_owned(),
            "wrap_link_actions".to_owned(),
        ]
    }
}

fn bazel_cc_escape_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '_' => escaped.push_str("_U"),
            '/' => escaped.push_str("_S"),
            '\\' => escaped.push_str("_B"),
            ':' => escaped.push_str("_C"),
            '@' => escaped.push_str("_A"),
            _ => escaped.push(c),
        }
    }
    escaped
}

fn bazel_cc_dynamic_library_soname(path: &str, preserve_name: bool, mnemonic: &str) -> String {
    if preserve_name {
        return path.rsplit('/').next().unwrap_or(path).to_owned();
    }

    let mnemonic_mangling = mnemonic
        .find("ST-")
        .map(|idx| format!("{}_", &mnemonic[idx..]))
        .unwrap_or_default();
    format!("lib{}{}", mnemonic_mangling, bazel_cc_escape_path(path))
}

fn bazel_cc_maybe_hash_preserve_extension(filename: &str) -> String {
    const MAX_FILENAME_LENGTH: usize = 255;
    if filename.len() <= MAX_FILENAME_LENGTH {
        return filename.to_owned();
    }

    let hashed_name = hex::encode(sha2::Sha256::digest(filename.as_bytes()));
    match filename.rsplit_once('.') {
        Some((_, extension)) if !extension.is_empty() => {
            format!("{hashed_name}.{extension}")
        }
        _ => hashed_name,
    }
}

fn bazel_cc_join_path(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.trim_start_matches('/').to_owned()
    } else if right.is_empty() {
        left.trim_end_matches('/').to_owned()
    } else {
        format!(
            "{}/{}",
            left.trim_end_matches('/'),
            right.trim_start_matches('/')
        )
    }
}

fn bazel_cc_dynamic_library_symlink_name(
    label: Option<&str>,
    solib_directory: &str,
    library_path: &str,
    preserve_name: bool,
    prefix_consumer: bool,
) -> String {
    let escaped_rule_path = bazel_cc_escape_path(&format!("_{}", label.unwrap_or("")));
    let soname = bazel_cc_dynamic_library_soname(library_path, preserve_name, "");
    if preserve_name {
        let library_parent = library_path
            .rsplit_once('/')
            .map(|(parent, _)| parent)
            .unwrap_or("");
        let escaped_library_path = bazel_cc_escape_path(&format!("_{library_parent}"));
        let escaped_full_path = if prefix_consumer {
            format!("{escaped_rule_path}__{escaped_library_path}")
        } else {
            escaped_library_path
        };
        let mangled_dir = bazel_cc_join_path(
            solib_directory,
            &bazel_cc_maybe_hash_preserve_extension(&escaped_full_path),
        );
        bazel_cc_join_path(&mangled_dir, &soname)
    } else {
        let filename = if prefix_consumer {
            format!("{escaped_rule_path}__{soname}")
        } else {
            soname
        };
        bazel_cc_join_path(
            solib_directory,
            &bazel_cc_maybe_hash_preserve_extension(&filename),
        )
    }
}

fn bazel_cc_library_root_relative_path<'v>(
    library: &'v dyn StarlarkInputArtifactLike<'v>,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    Ok(library
        .with_bazel_short_path(&|path| heap.alloc_str(path))?
        .as_str()
        .to_owned())
}

fn bazel_cc_input_artifact_like<'v>(
    library: &ValueAsInputArtifactLikeUnpack<'v>,
) -> &'v dyn StarlarkInputArtifactLike<'v> {
    match library {
        ValueAsInputArtifactLikeUnpack::Artifact(artifact) => *artifact,
        ValueAsInputArtifactLikeUnpack::DeclaredArtifact(artifact) => *artifact,
        ValueAsInputArtifactLikeUnpack::PromiseArtifact(artifact) => *artifact,
    }
}

fn bazel_cc_register_solib_symlink<'v>(
    actions: ValueTyped<'v, AnalysisActions<'v>>,
    library: ValueAsInputArtifactLikeUnpack<'v>,
    path: String,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
    let library_like = bazel_cc_input_artifact_like(&library);
    let library_path = bazel_cc_library_root_relative_path(library_like, eval.heap())?;
    let associated_artifacts = library_like
        .get_associated_artifacts()
        .cloned()
        .unwrap_or_else(AssociatedArtifacts::new);
    let bazel_owner = actions.as_ref().bazel_owner();
    let mut state = actions.as_ref().state()?;
    let artifact = state.declare_bazel_shareable_output(
        &path,
        OutputType::Symlink,
        eval.call_stack_top_location(),
        BuckOutPathKind::Configuration,
        bazel_owner,
        BazelOutputRoot::Bin,
        BazelOutputPathKind::OutputDirRelative,
        eval.heap(),
    )?;
    let output_artifact = artifact.as_output();
    let action_signature = format!("SolibSymlink:{library_path}");
    match &library {
        ValueAsInputArtifactLikeUnpack::DeclaredArtifact(declared) => {
            state.register_bazel_solib_symlink_action(
                declared.declared_artifact(),
                output_artifact,
                action_signature,
            )?;
        }
        _ => {
            let artifact_group = library_like.get_artifact_group()?;
            if state.should_register_bazel_shareable_action(&output_artifact, action_signature)? {
                state.register_action(
                    buck_indexset![output_artifact],
                    UnregisteredSolibSymlinkAction::new(artifact_group),
                    None,
                    None,
                )?;
            }
        }
    }
    Ok(StarlarkDeclaredArtifact::new(
        eval.call_stack_top_location(),
        artifact,
        associated_artifacts,
    ))
}

fn bazel_cc_runtime_solib_symlink_path<'v>(
    artifact: &ValueAsInputArtifactLikeUnpack<'v>,
    solib_directory: &str,
    runtime_solib_dir_base: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    let runtime_solib_dir_base = if runtime_solib_dir_base.is_none() {
        solib_directory
    } else {
        runtime_solib_dir_base.unpack_str().ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected `runtime_solib_dir_base` to be a string, got `{}`",
                runtime_solib_dir_base.get_type()
            ))
        })?
    };
    let library_path =
        bazel_cc_library_root_relative_path(bazel_cc_input_artifact_like(artifact), heap)?;
    let library_basename = library_path.rsplit('/').next().unwrap_or(&library_path);
    Ok(bazel_cc_join_path(runtime_solib_dir_base, library_basename))
}

fn bazel_cc_build_variable_from_dict<'v>(variables: Value<'v>, name: &str) -> Option<Value<'v>> {
    let dict = DictRef::from_value(variables)?;
    dict.iter()
        .find_map(|(key, value)| (key.unpack_str() == Some(name)).then_some(value))
}

fn bazel_cc_build_variable<'v>(variables: Value<'v>, name: &str) -> Option<Value<'v>> {
    BazelCcToolchainVariables::from_value(variables)
        .and_then(|variables| variables.value(name))
        .or_else(|| bazel_cc_build_variable_from_dict(variables, name))
}

fn bazel_cc_toolchain_variables_from_dict<'v>(
    variables: Value<'v>,
) -> starlark::Result<Box<[(String, Value<'v>)]>> {
    let Some(dict) = DictRef::from_value(variables) else {
        return Err(bazel_cc_error(format!(
            "Expected CcToolchainVariables vars to be a dict, got `{}`",
            variables.get_type()
        )));
    };
    let mut values = Vec::with_capacity(dict.len());
    for (key, value) in dict.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(bazel_cc_error(format!(
                "Expected CcToolchainVariables key to be a string, got `{}`",
                key.get_type()
            )));
        };
        values.push((key.to_owned(), value));
    }
    values.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(values.into_boxed_slice())
}

fn bazel_cc_extend_local_toolchain_variables<'v>(
    variables: Value<'v>,
    values: &mut Vec<(String, Value<'v>)>,
) -> starlark::Result<()> {
    if let Some(variables) = BazelCcToolchainVariables::from_value(variables) {
        for (key, value) in variables.local_values() {
            values.push((key.to_owned(), value));
        }
        return Ok(());
    }

    let Some(dict) = DictRef::from_value(variables) else {
        return Err(bazel_cc_error(format!(
            "Expected CcToolchainVariables, got `{}`",
            variables.get_type()
        )));
    };
    for (key, value) in dict.iter() {
        let Some(key) = key.unpack_str() else {
            return Err(bazel_cc_error(format!(
                "Expected CcToolchainVariables key to be a string, got `{}`",
                key.get_type()
            )));
        };
        values.push((key.to_owned(), value));
    }
    Ok(())
}

fn bazel_cc_check_duplicate_toolchain_variables(
    values: &[(String, Value<'_>)],
) -> starlark::Result<()> {
    for pair in values.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(bazel_cc_error(format!(
                "Cannot overwrite existing variables: {}",
                pair[0].0
            )));
        }
    }
    Ok(())
}

fn bazel_cc_link_sequence_values<'v>(
    value: Value<'v>,
    field: &str,
) -> starlark::Result<Vec<Value<'v>>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    if BazelDepset::from_value(value).is_some() {
        return bazel_depset_to_list(value);
    }
    bazel_cc_sequence_values(value, field)
}

fn bazel_cc_link_string<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<String> {
    if let Some(value) = value.unpack_str() {
        return Ok(value.to_owned());
    }
    for attr in ["path", "short_path"] {
        if let Some(value) = value.get_attr(attr, heap)?
            && let Some(value) = value.unpack_str()
        {
            return Ok(value.to_owned());
        }
    }
    Err(bazel_cc_error(format!(
        "Expected link argument value to be a string or artifact-like value, got `{}`",
        value.get_type()
    )))
}

fn bazel_cc_collect_values<'v>(
    value: Value<'v>,
    values: &mut Vec<Value<'v>>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if BazelDepset::from_value(value).is_some() {
        for item in bazel_depset_to_list(value)? {
            bazel_cc_collect_values(item, values)?;
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            bazel_cc_collect_values(item, values)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            bazel_cc_collect_values(item, values)?;
        }
        return Ok(());
    }
    values.push(value);
    Ok(())
}

fn bazel_cc_collect_input_values<'v>(
    value: Value<'v>,
    inputs: &mut StarlarkCmdArgs<'v>,
    heap: Heap<'v>,
) -> starlark::Result<()> {
    inputs.add_bazel_hidden_value(value, heap)
}

fn bazel_cc_collect_attr_input_values<'v>(
    owner: Value<'v>,
    attr: &str,
    inputs: &mut StarlarkCmdArgs<'v>,
    heap: Heap<'v>,
) -> starlark::Result<()> {
    if owner.is_none() {
        return Ok(());
    }
    let Some(value) = owner.get_attr(attr, heap)? else {
        return Ok(());
    };
    bazel_cc_collect_input_values(value, inputs, heap)
}

fn bazel_cc_optional_bool_arg(value: Value<'_>, name: &str) -> starlark::Result<Option<bool>> {
    if value.is_none() {
        return Ok(None);
    }
    value.unpack_bool().map(Some).ok_or_else(|| {
        bazel_cc_error(format!(
            "Expected `{name}` to be a bool, got `{}`",
            value.get_type()
        ))
    })
}

fn bazel_cc_collect_output<'v>(
    value: Value<'v>,
    outputs: &mut Vec<ValueTyped<'v, StarlarkDeclaredArtifact<'v>>>,
) -> starlark::Result<()> {
    if value.is_none() {
        return Ok(());
    }
    if BazelDepset::from_value(value).is_some() {
        for item in bazel_depset_to_list(value)? {
            bazel_cc_collect_output(item, outputs)?;
        }
        return Ok(());
    }
    if let Some(list) = ListRef::from_value(value) {
        for item in list.iter() {
            bazel_cc_collect_output(item, outputs)?;
        }
        return Ok(());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        for item in tuple.iter() {
            bazel_cc_collect_output(item, outputs)?;
        }
        return Ok(());
    }
    outputs.push(ValueTyped::<StarlarkDeclaredArtifact>::new_err(value)?);
    Ok(())
}

fn bazel_cc_compile_output_path<'v>(
    label: Value<'v>,
    output_name: &str,
) -> starlark::Result<String> {
    let target_name = if let Some(label) = StarlarkProvidersLabel::from_value(label) {
        label.label().target().name().as_str()
    } else if let Some(label) = StarlarkConfiguredProvidersLabel::from_value(label) {
        label.label().target().name().as_str()
    } else {
        return Err(bazel_cc_error(format!(
            "Expected `label` to be a Label, got `{}`",
            label.get_type()
        )));
    };
    Ok(format!("_objs/{target_name}/{output_name}"))
}

fn bazel_cc_action_context_actions<'v>(
    action_construction_context: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<ValueTyped<'v, AnalysisActions<'v>>> {
    let Some(actions) = action_construction_context.get_attr("actions", heap)? else {
        return Err(bazel_cc_error(
            "Expected action_construction_context to expose an `actions` attribute",
        ));
    };
    ValueTyped::<AnalysisActions>::new_err(actions)
}

fn bazel_cc_exec_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "DARWIN"
    } else if cfg!(target_os = "linux") {
        "LINUX"
    } else if cfg!(target_os = "windows") {
        "WINDOWS"
    } else if cfg!(target_os = "freebsd") {
        "FREEBSD"
    } else if cfg!(target_os = "openbsd") {
        "OPENBSD"
    } else {
        "UNKNOWN"
    }
}

fn cc_internal_kw_value<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    name: &str,
    default: Value<'v>,
) -> Value<'v> {
    kwargs.get(name).copied().unwrap_or(default)
}

fn cc_internal_kw_value_or_default<'v>(
    kwargs: &SmallMap<String, Value<'v>>,
    name: &str,
    default: Value<'v>,
) -> Value<'v> {
    let value = cc_internal_kw_value(kwargs, name, default);
    if value.is_none() { default } else { value }
}

fn bazel_cc_artifact_path<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<String> {
    if let Some(value) = value.unpack_str() {
        return Ok(value.to_owned());
    }
    for attr in ["path", "short_path"] {
        if let Some(value) = value.get_attr(attr, heap)?
            && let Some(value) = value.unpack_str()
        {
            return Ok(value.to_owned());
        }
    }
    Err(bazel_cc_error(format!(
        "Expected C++ source to be an artifact-like value, got `{}`",
        value.get_type()
    )))
}

fn bazel_cc_action_name_for_source_path(path: &str) -> &'static str {
    if path.ends_with(".c") {
        "c-compile"
    } else if [".m"].iter().any(|ext| path.ends_with(ext)) {
        "objc-compile"
    } else if [".mm"].iter().any(|ext| path.ends_with(ext)) {
        "objc++-compile"
    } else if path.ends_with(".S") {
        "preprocess-assemble"
    } else if (path.ends_with(".s") && !path.ends_with(".pic.s")) || path.ends_with(".asm") {
        "assemble"
    } else if [".cc", ".cpp", ".cxx", ".c++", ".C", ".cu", ".cl"]
        .iter()
        .any(|ext| path.ends_with(ext))
    {
        "c++-compile"
    } else {
        "c++-compile"
    }
}

fn bazel_cc_compile_action_name<'v>(
    action_name: Value<'v>,
    source: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    if let Some(action_name) = action_name.unpack_str() {
        return Ok(action_name.to_owned());
    }
    let source_path = bazel_cc_artifact_path(source, heap)?;
    Ok(bazel_cc_action_name_for_source_path(&source_path).to_owned())
}

fn cc_internal_header_info_attr<'v>(
    header_info: Value<'v>,
    name: &str,
    default: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    if header_info.is_none() {
        return Ok(default);
    }
    Ok(header_info.get_attr(name, eval.heap())?.unwrap_or(default))
}

fn cc_internal_default_header_list<'v>(value: Value<'v>, empty_list: Value<'v>) -> Value<'v> {
    if value.is_none() { empty_list } else { value }
}

fn cc_internal_alloc_header_info_values<'v>(
    heap: Heap<'v>,
    header_module: Value<'v>,
    pic_header_module: Value<'v>,
    modular_public_headers: Value<'v>,
    modular_private_headers: Value<'v>,
    textual_headers: Value<'v>,
    separate_module_headers: Value<'v>,
    separate_module: Value<'v>,
    separate_pic_module: Value<'v>,
    deps: Value<'v>,
    merged_deps: Value<'v>,
) -> Value<'v> {
    let empty_list = heap.alloc(AllocList::EMPTY);
    heap.alloc(AllocStruct([
        ("header_module", header_module),
        ("pic_header_module", pic_header_module),
        (
            "modular_public_headers",
            cc_internal_default_header_list(modular_public_headers, empty_list),
        ),
        (
            "modular_private_headers",
            cc_internal_default_header_list(modular_private_headers, empty_list),
        ),
        (
            "textual_headers",
            cc_internal_default_header_list(textual_headers, empty_list),
        ),
        (
            "separate_module_headers",
            cc_internal_default_header_list(separate_module_headers, empty_list),
        ),
        ("separate_module", separate_module),
        ("separate_pic_module", separate_pic_module),
        ("deps", cc_internal_default_header_list(deps, empty_list)),
        (
            "merged_deps",
            cc_internal_default_header_list(merged_deps, empty_list),
        ),
    ]))
}

fn cc_internal_alloc_header_info_with_deps_values<'v>(
    header_info: Value<'v>,
    deps: Value<'v>,
    merged_deps: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let none = Value::new_none();
    let empty_list = eval.heap().alloc(AllocList::EMPTY);
    Ok(cc_internal_alloc_header_info_values(
        eval.heap(),
        cc_internal_header_info_attr(header_info, "header_module", none, eval)?,
        cc_internal_header_info_attr(header_info, "pic_header_module", none, eval)?,
        cc_internal_header_info_attr(header_info, "modular_public_headers", empty_list, eval)?,
        cc_internal_header_info_attr(header_info, "modular_private_headers", empty_list, eval)?,
        cc_internal_header_info_attr(header_info, "textual_headers", empty_list, eval)?,
        cc_internal_header_info_attr(header_info, "separate_module_headers", empty_list, eval)?,
        cc_internal_header_info_attr(header_info, "separate_module", none, eval)?,
        cc_internal_header_info_attr(header_info, "separate_pic_module", none, eval)?,
        deps,
        merged_deps,
    ))
}

#[starlark_module]
fn bazel_cc_toolchain_features_methods(builder: &mut MethodsBuilder) {
    fn default_features_and_action_configs<'v>(
        #[starlark(this)] this: &BazelCcToolchainFeatures,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(
            this.default_selectables
                .iter()
                .map(|value| heap.alloc_str(value).to_value()),
        )))
    }

    fn configure_features(
        #[starlark(this)] this: &BazelCcToolchainFeatures,
        #[starlark(require = named, default = UnpackList::default())]
        requested_features: UnpackList<String>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        let mut requested_features = requested_features.into_iter().collect::<Vec<_>>();
        requested_features.sort();
        requested_features.dedup();
        this.configure_features(requested_features)
    }
}

#[starlark_module]
fn bazel_feature_configuration_methods(builder: &mut MethodsBuilder) {
    fn is_enabled(
        #[starlark(this)] this: &BazelFeatureConfiguration,
        feature: &str,
    ) -> starlark::Result<bool> {
        Ok(this.is_enabled_selectable(feature))
    }

    fn is_requested(
        #[starlark(this)] this: &BazelFeatureConfiguration,
        feature: &str,
    ) -> starlark::Result<bool> {
        Ok(this
            .requested_features
            .iter()
            .any(|requested| requested == feature))
    }
}

#[starlark_module]
fn bazel_cc_internal_methods(builder: &mut MethodsBuilder) {
    fn cc_toolchain_features<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelCcToolchainFeatures> {
        let heap = eval.heap();
        let toolchain_config_info =
            cc_internal_kw_value(&kwargs, "toolchain_config_info", Value::new_none());
        let tools_directory = cc_internal_kw_value(&kwargs, "tools_directory", Value::new_none())
            .unpack_str()
            .unwrap_or("")
            .to_owned();
        bazel_cc_parse_toolchain_features(toolchain_config_info, tools_directory, heap)
    }

    fn configure_features<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named, default = NoneType)] language: Value<'v>,
        #[starlark(require = named, default = UnpackList::default())]
        requested_features: UnpackList<String>,
        #[starlark(require = named, default = UnpackList::default())]
        unsupported_features: UnpackList<String>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        bazel_cc_configure_features_from_ctx(
            ctx,
            cc_toolchain,
            language,
            requested_features.into_iter().collect(),
            unsupported_features.into_iter().collect(),
            eval,
        )
    }

    fn cc_toolchain_variables<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] vars: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if BazelCcToolchainVariables::from_value(vars).is_some() {
            return Ok(vars);
        }
        let values = bazel_cc_toolchain_variables_from_dict(vars)?;
        Ok(eval.heap().alloc(BazelCcToolchainVariables {
            parent: None,
            values,
        }))
    }

    fn combine_cc_toolchain_variables<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        parent: Value<'v>,
        #[starlark(args)] variables: UnpackTuple<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        if parent.is_none() {
            return Err(bazel_cc_error(
                "Expected parent CcToolchainVariables, got `NoneType`",
            ));
        }
        if BazelCcToolchainVariables::from_value(parent).is_none()
            && DictRef::from_value(parent).is_none()
        {
            return Err(bazel_cc_error(format!(
                "Expected parent CcToolchainVariables, got `{}`",
                parent.get_type()
            )));
        }

        let mut values = Vec::new();
        for variables in variables.items {
            bazel_cc_extend_local_toolchain_variables(variables, &mut values)?;
        }
        values.sort_by(|(left, _), (right, _)| left.cmp(right));
        bazel_cc_check_duplicate_toolchain_variables(&values)?;
        Ok(eval.heap().alloc(BazelCcToolchainVariables {
            parent: Some(parent),
            values: values.into_boxed_slice(),
        }))
    }

    fn intern_string_sequence_variable_value<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        string_sequence: UnpackList<String>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let values = string_sequence
            .into_iter()
            .map(|value| heap.alloc_str(&value))
            .collect::<Vec<_>>();
        Ok(heap.alloc(AllocTuple(values)))
    }

    fn intern_seq<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        seq: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocTuple(bazel_cc_sequence_values(seq, "seq")?)))
    }

    fn compute_output_name_prefix_dir<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] configuration: Value<'v>,
        #[starlark(require = named, default = NoneType)] purpose: Value<'v>,
    ) -> starlark::Result<&'static str> {
        let _unused = configuration;
        let mnemonic = purpose.unpack_str().unwrap_or("");
        if mnemonic.ends_with("_objc_arc") {
            if mnemonic.ends_with("_non_objc_arc") {
                Ok("non_arc")
            } else {
                Ok("arc")
            }
        } else {
            Ok("")
        }
    }

    fn is_tree_artifact<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        artifact: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let Some(is_directory) = artifact.get_attr("is_directory", eval.heap())? else {
            return Ok(false);
        };
        is_directory.unpack_bool().ok_or_else(|| {
            bazel_cc_error(format!(
                "Expected artifact.is_directory to be a bool, got `{}`",
                is_directory.get_type()
            ))
        })
    }

    fn get_artifact_name_for_category<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] category: &str,
        #[starlark(require = named)] output_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let category = bazel_cc_artifact_category(category)?;
        let features = bazel_cc_toolchain_features_from_toolchain(cc_toolchain, eval.heap())?;
        let (prefix, extension) = bazel_cc_artifact_name_pattern(features, category);
        Ok(bazel_cc_artifact_name(output_name, prefix, extension))
    }

    fn get_artifact_name_extension_for_category<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] category: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let category = bazel_cc_artifact_category(category)?;
        let features = bazel_cc_toolchain_features_from_toolchain(cc_toolchain, eval.heap())?;
        let (_, extension) = bazel_cc_artifact_name_pattern(features, category);
        Ok(extension.to_owned())
    }

    fn wrap_link_actions<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: Value<'v>,
        #[starlark(default = NoneType)] build_configuration: Value<'v>,
        #[starlark(default = false)] sharable_artifacts: bool,
    ) -> starlark::Result<Value<'v>> {
        let _unused = (build_configuration, sharable_artifacts);
        Ok(actions)
    }

    fn actions2ctx_cheat<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: ValueTyped<'v, AnalysisActions<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(analysis_actions_to_bazel_ctx(actions, eval.heap()))
    }

    fn rule_class<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        ctx: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        if let Some(rule_class) = ctx.get_attr("rule_class", eval.heap())? {
            if let Some(rule_class) = rule_class.unpack_str() {
                return Ok(rule_class.to_owned());
            }
        }
        if let Some(rule) = ctx.get_attr("rule", eval.heap())? {
            if let Some(kind) = rule.get_attr("kind", eval.heap())? {
                if let Some(kind) = kind.unpack_str() {
                    return Ok(kind.to_owned());
                }
            }
        }
        Ok(String::new())
    }

    fn declare_compile_output_file<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] label: Value<'v>,
        #[starlark(require = named)] output_name: &str,
        #[starlark(require = named)] configuration: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let _unused = configuration;
        let actions = bazel_cc_action_context_actions(ctx, eval.heap())?;
        let path = bazel_cc_compile_output_path(label, output_name)?;
        let artifact = actions
            .as_ref()
            .state()?
            .declare_output_with_bazel_owner_and_output_root(
                None,
                &path,
                OutputType::File,
                eval.call_stack_top_location(),
                BuckOutPathKind::Configuration,
                actions.as_ref().bazel_owner(),
                actions.as_ref().bazel_output_root,
                eval.heap(),
            )?;
        Ok(StarlarkDeclaredArtifact::new(
            eval.call_stack_top_location(),
            artifact,
            AssociatedArtifacts::new(),
        ))
    }

    fn create_header_info<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named, default = NoneType)] header_module: Value<'v>,
        #[starlark(require = named, default = NoneType)] pic_header_module: Value<'v>,
        #[starlark(require = named, default = NoneType)] modular_public_headers: Value<'v>,
        #[starlark(require = named, default = NoneType)] modular_private_headers: Value<'v>,
        #[starlark(require = named, default = NoneType)] textual_headers: Value<'v>,
        #[starlark(require = named, default = NoneType)] separate_module_headers: Value<'v>,
        #[starlark(require = named, default = NoneType)] separate_module: Value<'v>,
        #[starlark(require = named, default = NoneType)] separate_pic_module: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(cc_internal_alloc_header_info_values(
            eval.heap(),
            header_module,
            pic_header_module,
            modular_public_headers,
            modular_private_headers,
            textual_headers,
            separate_module_headers,
            separate_module,
            separate_pic_module,
            Value::new_none(),
            Value::new_none(),
        ))
    }

    fn create_header_info_with_deps<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named, default = NoneType)] header_info: Value<'v>,
        #[starlark(require = named, default = NoneType)] deps: Value<'v>,
        #[starlark(require = named, default = NoneType)] merged_deps: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        cc_internal_alloc_header_info_with_deps_values(header_info, deps, merged_deps, eval)
    }

    fn dynamic_library_soname<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: Value<'v>,
        path: &str,
        preserve_name: bool,
    ) -> starlark::Result<String> {
        let _unused = actions;
        Ok(bazel_cc_dynamic_library_soname(path, preserve_name, ""))
    }

    fn dynamic_library_symlink<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: ValueTyped<'v, AnalysisActions<'v>>,
        library: ValueAsInputArtifactLikeUnpack<'v>,
        solib_directory: &str,
        preserve_name: bool,
        prefix_consumer: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let library_path = bazel_cc_library_root_relative_path(
            bazel_cc_input_artifact_like(&library),
            eval.heap(),
        )?;
        let label = actions
            .as_ref()
            .bazel_owner()
            .map(|owner| owner.unconfigured().to_string());
        let path = bazel_cc_dynamic_library_symlink_name(
            label.as_deref(),
            solib_directory,
            &library_path,
            preserve_name,
            prefix_consumer,
        );
        bazel_cc_register_solib_symlink(actions, library, path, eval)
    }

    fn dynamic_library_symlink2<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        actions: ValueTyped<'v, AnalysisActions<'v>>,
        library: ValueAsInputArtifactLikeUnpack<'v>,
        solib_directory: &str,
        path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let path = bazel_cc_join_path(solib_directory, path);
        bazel_cc_register_solib_symlink(actions, library, path, eval)
    }

    fn solib_symlink_action<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] artifact: ValueAsInputArtifactLikeUnpack<'v>,
        #[starlark(require = named)] solib_directory: &str,
        #[starlark(require = named)] runtime_solib_dir_base: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<StarlarkDeclaredArtifact<'v>> {
        let actions = bazel_cc_action_context_actions(ctx, eval.heap())?;
        let path = bazel_cc_runtime_solib_symlink_path(
            &artifact,
            solib_directory,
            runtime_solib_dir_base,
            eval.heap(),
        )?;
        bazel_cc_register_solib_symlink(actions, artifact, path, eval)
    }

    fn get_link_args<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
        #[starlark(require = named)] build_variables: Value<'v>,
        #[starlark(require = named, default = NoneType)] parameter_file_type: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let args = bazel_cc_feature_command_line(
            feature_configuration.as_ref(),
            action_name,
            build_variables,
            eval.heap(),
        )?;
        bazel_cc_link_param_file(args, build_variables, parameter_file_type, eval.heap())
    }

    fn per_file_copts<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        cpp_configuration: Value<'v>,
        source_file: Value<'v>,
        label: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let _unused = (cpp_configuration, source_file, label);
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn create_cc_compile_action<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(require = named)] action_construction_context: Value<'v>,
        #[starlark(require = named)] cc_compilation_context: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] compile_build_variables: Value<'v>,
        #[starlark(require = named)] source: Value<'v>,
        #[starlark(require = named)] output_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] additional_compilation_inputs: Value<'v>,
        #[starlark(require = named, default = NoneType)] additional_compilation_inputs_set: Value<
            'v,
        >,
        #[starlark(require = named, default = NoneType)] additional_include_scanning_roots: Value<
            'v,
        >,
        #[starlark(require = named, default = NoneType)] diagnostics_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] dotd_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] gcno_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] dwo_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] lto_indexing_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] action_name: Value<'v>,
        #[starlark(require = named, default = NoneType)] additional_outputs: Value<'v>,
        #[starlark(require = named, default = NoneType)] module_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] modmap_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] modmap_input_file: Value<'v>,
        #[starlark(require = named, default = NoneType)] configuration: Value<'v>,
        #[starlark(require = named, default = NoneType)] copts_filter: Value<'v>,
        #[starlark(require = named, default = false)] use_pic: bool,
        #[starlark(require = named, default = NoneType)] needs_include_validation: Value<'v>,
        #[starlark(require = named, default = NoneType)] cache_key_inputs: Value<'v>,
        #[starlark(require = named, default = NoneType)] build_info_header_files: Value<'v>,
        #[starlark(require = named, default = NoneType)] additional_prunable_headers: Value<'v>,
        #[starlark(require = named, default = NoneType)] should_scan_includes: Value<'v>,
        #[starlark(require = named, default = NoneType)] shareable: Value<'v>,
        #[starlark(require = named, default = NoneType)] toolchain_type: Value<'v>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let heap = eval.heap();
        let actions = bazel_cc_action_context_actions(action_construction_context, heap)?;
        let feature_configuration = feature_configuration.as_ref();
        let action_name = bazel_cc_compile_action_name(action_name, source, heap)?;
        let tool_path = feature_configuration
            .selected_tool(&action_name)?
            .tool_path(&feature_configuration.data.tools_directory);
        let executable = heap.alloc_str(&tool_path).to_value();
        let should_scan_includes =
            bazel_cc_optional_bool_arg(should_scan_includes, "should_scan_includes")?
                .unwrap_or(false);
        let _shareable = bazel_cc_optional_bool_arg(shareable, "shareable")?;
        let command_line = heap.alloc_typed(BazelCcCompileCommandLine {
            feature_configuration: Arc::clone(&feature_configuration.data),
            action_name: action_name.clone(),
            variables: ValueOfUnchecked::new(compile_build_variables),
        });

        let mut inputs = StarlarkCmdArgs::default();
        bazel_cc_collect_input_values(source, &mut inputs, heap)?;
        bazel_cc_collect_input_values(additional_compilation_inputs, &mut inputs, heap)?;
        bazel_cc_collect_input_values(additional_compilation_inputs_set, &mut inputs, heap)?;
        bazel_cc_collect_input_values(additional_include_scanning_roots, &mut inputs, heap)?;
        bazel_cc_collect_input_values(cache_key_inputs, &mut inputs, heap)?;
        bazel_cc_collect_input_values(build_info_header_files, &mut inputs, heap)?;
        for attr in ["headers", "_non_code_inputs"] {
            bazel_cc_collect_attr_input_values(cc_compilation_context, attr, &mut inputs, heap)?;
        }
        if !should_scan_includes && dotd_file.is_none() {
            bazel_cc_collect_input_values(additional_prunable_headers, &mut inputs, heap)?;
        }
        if let Some(module_map) = cc_compilation_context.get_attr("_module_map", heap)? {
            bazel_cc_collect_attr_input_values(module_map, "file", &mut inputs, heap)?;
        }
        if let Some(module_maps) = cc_compilation_context.get_attr("_direct_module_maps", heap)? {
            let mut module_maps_list = Vec::new();
            bazel_cc_collect_values(module_maps, &mut module_maps_list)?;
            for module_map in module_maps_list {
                bazel_cc_collect_attr_input_values(module_map, "file", &mut inputs, heap)?;
            }
        }
        let toolchain_files = if should_scan_includes {
            "_compiler_files_without_includes"
        } else if action_name == "assemble" {
            "_as_files"
        } else {
            "_compiler_files"
        };
        bazel_cc_collect_attr_input_values(cc_toolchain, toolchain_files, &mut inputs, heap)?;
        if should_scan_includes {
            // Bazel falls back to compiler_files when compiler_files_without_includes is empty.
            // Keeping both here is a safe over-approximation for the rare include-scanning case.
            bazel_cc_collect_attr_input_values(cc_toolchain, "_compiler_files", &mut inputs, heap)?;
        }
        bazel_cc_collect_attr_input_values(
            cc_toolchain,
            "_builtin_include_files",
            &mut inputs,
            heap,
        )?;
        for variable in [
            "module_map_file",
            "dependent_module_map_files",
            "thinlto_index",
            "thinlto_input_bitcode_file",
            "input_file",
        ] {
            if let Some(value) = bazel_cc_build_variable(compile_build_variables, variable) {
                bazel_cc_collect_input_values(value, &mut inputs, heap)?;
            }
        }

        let mut outputs = Vec::new();
        for value in [
            output_file,
            dotd_file,
            diagnostics_file,
            gcno_file,
            dwo_file,
            lto_indexing_file,
            additional_outputs,
            module_files,
            modmap_file,
            modmap_input_file,
        ] {
            bazel_cc_collect_output(value, &mut outputs)?;
        }

        let _unused = (
            configuration,
            copts_filter,
            use_pic,
            needs_include_validation,
            additional_prunable_headers,
            toolchain_type,
        );

        let mnemonic = heap.alloc_str("CppCompile");
        (BAZEL_CC_CREATE_COMPILE_ACTION.get()?)(
            BazelCcCompileAction {
                actions,
                executable,
                command_line,
                inputs,
                outputs,
                mnemonic,
            },
            eval,
        )
    }

    fn exec_os<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        ctx: Value<'v>,
    ) -> starlark::Result<&'static str> {
        let _unused = ctx;
        Ok(bazel_cc_exec_os())
    }

    fn freeze<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        value: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(value)
    }

    fn check_private_api<'v>(
        #[starlark(this)] _this: &BazelCcInternal,
        #[starlark(args)] _args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<NoneType> {
        Ok(NoneType)
    }
}

#[starlark_module]
fn bazel_cc_common_module(builder: &mut GlobalsBuilder) {
    fn internal_DO_NOT_USE() -> starlark::Result<BazelCcInternal> {
        Ok(BazelCcInternal)
    }

    fn configure_features<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named, default = NoneType)] language: Value<'v>,
        #[starlark(require = named, default = UnpackList::default())]
        requested_features: UnpackList<String>,
        #[starlark(require = named, default = UnpackList::default())]
        unsupported_features: UnpackList<String>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<BazelFeatureConfiguration> {
        let _unused = (ctx, language);
        let unsupported_features = unsupported_features.into_iter().collect::<Vec<_>>();
        let mut requested_features = requested_features
            .into_iter()
            .filter(|feature| !unsupported_features.contains(feature))
            .collect::<Vec<_>>();
        requested_features.sort();
        requested_features.dedup();
        let features = bazel_cc_toolchain_features_from_toolchain(cc_toolchain, eval.heap())?;
        features.configure_features(requested_features)
    }

    fn is_cc_toolchain_resolution_enabled_do_not_use<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
    ) -> starlark::Result<bool> {
        let _unused = ctx;
        Ok(true)
    }

    fn get_tool_for_action<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
    ) -> starlark::Result<String> {
        Ok(feature_configuration
            .selected_tool(action_name)?
            .tool_path(&feature_configuration.data.tools_directory))
    }

    fn get_execution_requirements<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(
            feature_configuration
                .selected_tool(action_name)?
                .execution_requirements
                .iter()
                .map(|value| heap.alloc_str(value).to_value()),
        )))
    }

    fn action_is_enabled<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
    ) -> starlark::Result<bool> {
        Ok(feature_configuration.action_is_configured(action_name))
    }

    fn get_memory_inefficient_command_line<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let none = Value::new_none();
        let feature_configuration = cc_internal_kw_value(&kwargs, "feature_configuration", none)
            .downcast_ref::<BazelFeatureConfiguration>()
            .ok_or_else(|| {
                bazel_cc_error("Expected feature_configuration to be a FeatureConfiguration")
            })?;
        let action_name = cc_internal_kw_value(&kwargs, "action_name", none)
            .unpack_str()
            .ok_or_else(|| bazel_cc_error("Expected action_name to be a string"))?;
        let variables = cc_internal_kw_value(&kwargs, "variables", none);
        Ok(eval.heap().alloc(AllocList(bazel_cc_feature_command_line(
            feature_configuration,
            action_name,
            variables,
            eval.heap(),
        )?)))
    }

    fn get_environment_variables<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let none = Value::new_none();
        let feature_configuration = cc_internal_kw_value(&kwargs, "feature_configuration", none)
            .downcast_ref::<BazelFeatureConfiguration>()
            .ok_or_else(|| {
                bazel_cc_error("Expected feature_configuration to be a FeatureConfiguration")
            })?;
        let action_name = cc_internal_kw_value(&kwargs, "action_name", none)
            .unpack_str()
            .ok_or_else(|| bazel_cc_error("Expected action_name to be a string"))?;
        let variables = cc_internal_kw_value(&kwargs, "variables", none);
        Ok(eval.heap().alloc(AllocDict(bazel_cc_feature_environment(
            feature_configuration,
            action_name,
            variables,
            eval.heap(),
        )?)))
    }

    fn empty_variables<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn create_compile_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn create_link_variables<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())))
    }

    fn legacy_cc_flags_make_variable_do_not_use<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocList::EMPTY))
    }

    fn incompatible_disable_objc_library_transition() -> starlark::Result<bool> {
        Ok(false)
    }

    fn add_go_exec_groups_to_binary_rules() -> starlark::Result<bool> {
        Ok(false)
    }

    fn check_experimental_cc_shared_library() -> starlark::Result<bool> {
        Ok(false)
    }

    fn get_tool_requirement_for_action<'v>(
        #[starlark(require = named)] feature_configuration: ValueTyped<
            'v,
            BazelFeatureConfiguration,
        >,
        #[starlark(require = named)] action_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        Ok(heap.alloc(AllocList(
            feature_configuration
                .selected_tool(action_name)?
                .execution_requirements
                .iter()
                .map(|value| heap.alloc_str(value).to_value()),
        )))
    }

    fn implementation_deps_allowed_by_allowlist<'v>(
        #[starlark(kwargs)] _kwargs: SmallMap<String, Value<'v>>,
    ) -> starlark::Result<bool> {
        Ok(true)
    }

    fn create_cc_toolchain_config_info<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<CcNativeProvider<'v>> {
        let empty_list = eval.heap().alloc(AllocList::EMPTY);
        let empty_string = eval.heap().alloc("");
        Ok(make_cc_native_provider(
            CC_TOOLCHAIN_CONFIG_INFO,
            CcNativeProviderCallable::new(CC_TOOLCHAIN_CONFIG_INFO).id,
            [
                (
                    "_action_configs_DO_NOT_USE".to_owned(),
                    cc_internal_kw_value(&kwargs, "action_configs", empty_list),
                ),
                (
                    "_artifact_name_patterns_DO_NOT_USE".to_owned(),
                    cc_internal_kw_value(&kwargs, "artifact_name_patterns", empty_list),
                ),
                (
                    "_exec_os_DO_NOT_USE".to_owned(),
                    eval.heap().alloc(bazel_cc_exec_os()),
                ),
                (
                    "_features_DO_NOT_USE".to_owned(),
                    cc_internal_kw_value(&kwargs, "features", empty_list),
                ),
                (
                    "abi_libc_version".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "abi_libc_version", empty_string),
                ),
                (
                    "abi_version".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "abi_version", empty_string),
                ),
                (
                    "builtin_sysroot".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "builtin_sysroot", empty_string),
                ),
                (
                    "compiler".to_owned(),
                    cc_internal_kw_value(&kwargs, "compiler", empty_string),
                ),
                (
                    "cxx_builtin_include_directories".to_owned(),
                    cc_internal_kw_value(&kwargs, "cxx_builtin_include_directories", empty_list),
                ),
                (
                    "make_variables".to_owned(),
                    cc_internal_kw_value(&kwargs, "make_variables", empty_list),
                ),
                (
                    "target_cpu".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "target_cpu", empty_string),
                ),
                (
                    "target_libc".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "target_libc", empty_string),
                ),
                (
                    "target_system_name".to_owned(),
                    cc_internal_kw_value_or_default(&kwargs, "target_system_name", empty_string),
                ),
                (
                    "tool_paths".to_owned(),
                    cc_internal_kw_value(&kwargs, "tool_paths", empty_list),
                ),
                (
                    "toolchain_id".to_owned(),
                    cc_internal_kw_value(&kwargs, "toolchain_identifier", empty_string),
                ),
            ],
        ))
    }
}

pub(crate) fn register_cc_common(globals: &mut GlobalsBuilder) {
    globals.set(
        DEBUG_PACKAGE_INFO,
        CcNativeProviderCallable::new(DEBUG_PACKAGE_INFO),
    );
    globals.set(
        CC_TOOLCHAIN_CONFIG_INFO,
        CcNativeProviderCallable::new(CC_TOOLCHAIN_CONFIG_INFO),
    );
    globals.set(
        CC_SHARED_LIBRARY_INFO,
        CcNativeProviderCallable::new(CC_SHARED_LIBRARY_INFO),
    );
    globals.set(
        CC_SHARED_LIBRARY_HINT_INFO,
        CcNativeProviderCallable::new(CC_SHARED_LIBRARY_HINT_INFO),
    );
    globals.namespace("cc_common", |cc_common| {
        cc_common.set(
            CC_TOOLCHAIN_INFO,
            CcNativeProviderCallable::new(CC_TOOLCHAIN_INFO),
        );
        cc_common.set("do_not_use_tools_cpp_compiler_present", NoneType);
        bazel_cc_common_module(cc_common);
    });
}

use std::fmt;
use std::sync::Arc;

use allocative::Allocative;
use buck2_core::provider::id::ProviderId;
use buck2_interpreter::types::provider::callable::ProviderCallableLike;
use buck2_interpreter::types::provider::callable::ProviderLike;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::eval::Arguments;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::Demand;
use starlark::values::Freeze;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::StringValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLifetimeless;
use starlark::values::ValueLike;
use starlark::values::dict::AllocDict;
use starlark::values::none::NoneType;
use starlark::values::starlark_value;
use starlark::values::structs::AllocStruct;

fn apple_platform(
    globals: &GlobalsBuilder,
    name: &'static str,
    platform_type: &'static str,
    is_device: bool,
    name_in_plist: &'static str,
) -> AllocStruct<[(&'static str, FrozenValue); 4]> {
    AllocStruct([
        ("name", globals.alloc(name)),
        ("platform_type", globals.alloc(platform_type)),
        ("is_device", globals.alloc(is_device)),
        ("name_in_plist", globals.alloc(name_in_plist)),
    ])
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelAppleToolchain;

impl fmt::Display for BazelAppleToolchain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("apple_toolchain")
    }
}

starlark::starlark_simple_value!(BazelAppleToolchain);

#[starlark_value(type = "apple_toolchain")]
impl<'v> StarlarkValue<'v> for BazelAppleToolchain {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_apple_toolchain_methods)
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "developer_dir".to_owned(),
            "platform_developer_framework_dir".to_owned(),
            "sdk_dir".to_owned(),
        ]
    }
}

#[starlark_module]
fn bazel_apple_toolchain_methods(builder: &mut MethodsBuilder) {
    fn sdk_dir(#[starlark(this)] _this: &BazelAppleToolchain) -> starlark::Result<&'static str> {
        Ok("__BAZEL_XCODE_SDKROOT__")
    }

    fn developer_dir(
        #[starlark(this)] _this: &BazelAppleToolchain,
    ) -> starlark::Result<&'static str> {
        Ok("__BAZEL_XCODE_DEVELOPER_DIR__")
    }

    fn platform_developer_framework_dir<'v>(
        #[starlark(this)] _this: &BazelAppleToolchain,
        apple_fragment: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let platform = apple_fragment.get_attr_error("single_arch_platform", eval.heap())?;
        let name_in_plist = platform.get_attr_error("name_in_plist", eval.heap())?;
        let name_in_plist = name_in_plist.unpack_str().unwrap_or("MacOSX");
        Ok(format!(
            "__BAZEL_XCODE_DEVELOPER_DIR__/Platforms/{name_in_plist}.platform/Developer/Library/Frameworks"
        ))
    }
}

#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative)]
struct BazelAppleProviderCallable {
    name: &'static str,
    id: Arc<ProviderId>,
}

impl BazelAppleProviderCallable {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            id: Arc::new(ProviderId {
                path: None,
                name: name.to_owned(),
            }),
        }
    }
}

impl fmt::Display for BazelAppleProviderCallable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

starlark::starlark_simple_value!(BazelAppleProviderCallable);

#[starlark_value(type = "bazel_apple_provider_callable")]
impl<'v> StarlarkValue<'v> for BazelAppleProviderCallable {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let kwargs = args.names_map()?;
        if self.name == "XcodeVersionConfig" {
            let provider = bazel_xcode_version_config_from_kwargs(self.id.clone(), kwargs, eval);
            return Ok(eval.heap().alloc(provider).to_value());
        }
        Ok(eval
            .heap()
            .alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new()))
            .to_value())
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderCallableLike>(self);
    }
}

impl ProviderCallableLike for BazelAppleProviderCallable {
    fn id(&self) -> buck2_error::Result<&Arc<ProviderId>> {
        Ok(&self.id)
    }
}

#[derive(Clone, Debug, Freeze, ProvidesStaticType, Trace, Allocative)]
#[repr(C)]
pub struct BazelXcodeVersionConfigGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[trace(unsafe_ignore)]
    id: Arc<ProviderId>,
    xcode_version: V,
    ios_sdk_version: V,
    ios_minimum_os_version: V,
    visionos_sdk_version: V,
    visionos_minimum_os_version: V,
    watchos_sdk_version: V,
    watchos_minimum_os_version: V,
    tvos_sdk_version: V,
    tvos_minimum_os_version: V,
    macos_sdk_version: V,
    macos_minimum_os_version: V,
    availability: V,
    include_xcode_execution_info: bool,
}

starlark_complex_value!(pub BazelXcodeVersionConfig);

unsafe impl<FromV, ToV> Coerce<BazelXcodeVersionConfigGen<ToV>>
    for BazelXcodeVersionConfigGen<FromV>
where
    FromV: ValueLifetimeless + Coerce<ToV>,
    ToV: ValueLifetimeless,
{
}

impl<V: ValueLifetimeless> fmt::Display for BazelXcodeVersionConfigGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("XcodeVersionConfig(<computed>)")
    }
}

impl<'v, V: ValueLike<'v>> serde::Serialize for BazelXcodeVersionConfigGen<V> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.collect_map(self.items())
    }
}

#[starlark_value(type = "XcodeVersionConfig")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for BazelXcodeVersionConfigGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn dir_attr(&self) -> Vec<String> {
        vec![
            "availability".to_owned(),
            "execution_info".to_owned(),
            "minimum_os_for_platform_type".to_owned(),
            "sdk_version_for_platform".to_owned(),
            "xcode_version".to_owned(),
        ]
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods_for_type::<Self::Canonical>(bazel_xcode_version_config_methods)
    }

    fn provide(&'v self, demand: &mut Demand<'_, 'v>) {
        demand.provide_value::<&dyn ProviderLike>(self);
    }
}

impl<'v, V: ValueLike<'v>> ProviderLike<'v> for BazelXcodeVersionConfigGen<V> {
    fn id(&self) -> &Arc<ProviderId> {
        &self.id
    }

    fn items(&self) -> Vec<(&str, Value<'v>)> {
        vec![
            ("xcode_version", self.xcode_version.to_value()),
            ("ios_sdk_version", self.ios_sdk_version.to_value()),
            (
                "ios_minimum_os_version",
                self.ios_minimum_os_version.to_value(),
            ),
            ("macos_sdk_version", self.macos_sdk_version.to_value()),
            (
                "macos_minimum_os_version",
                self.macos_minimum_os_version.to_value(),
            ),
            ("availability", self.availability.to_value()),
        ]
    }
}

fn bazel_apple_error(message: impl Into<String>) -> starlark::Error {
    starlark::Error::new_other(std::io::Error::other(message.into()))
}

fn value_str<'v>(value: Value<'v>) -> Option<&'v str> {
    let value = value.to_value();
    if value.is_none() {
        return None;
    }
    value.unpack_str()
}

fn kwarg_or_default<'v>(
    kwargs: &SmallMap<StringValue<'v>, Value<'v>>,
    heap: Heap<'v>,
    name: &str,
    default: Option<&str>,
) -> Value<'v> {
    kwargs
        .iter()
        .find_map(|(key, value)| (key.as_str() == name).then_some(*value))
        .unwrap_or_else(|| {
            default
                .map(|value| heap.alloc_str(value).to_value())
                .unwrap_or_else(Value::new_none)
        })
}

fn bazel_xcode_version_config_from_kwargs<'v>(
    id: Arc<ProviderId>,
    kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> BazelXcodeVersionConfig<'v> {
    let heap = eval.heap();
    let include_xcode_execution_info =
        kwarg_or_default(&kwargs, heap, "include_xcode_execution_info", Some("false"))
            .unpack_bool()
            .unwrap_or(false);
    BazelXcodeVersionConfig {
        id,
        xcode_version: kwarg_or_default(&kwargs, heap, "xcode_version", None),
        ios_sdk_version: kwarg_or_default(&kwargs, heap, "ios_sdk_version", Some("0.0")),
        ios_minimum_os_version: kwarg_or_default(
            &kwargs,
            heap,
            "ios_minimum_os_version",
            Some("0.0"),
        ),
        visionos_sdk_version: kwarg_or_default(&kwargs, heap, "visionos_sdk_version", Some("0.0")),
        visionos_minimum_os_version: kwarg_or_default(
            &kwargs,
            heap,
            "visionos_minimum_os_version",
            Some("0.0"),
        ),
        watchos_sdk_version: kwarg_or_default(&kwargs, heap, "watchos_sdk_version", Some("0.0")),
        watchos_minimum_os_version: kwarg_or_default(
            &kwargs,
            heap,
            "watchos_minimum_os_version",
            Some("0.0"),
        ),
        tvos_sdk_version: kwarg_or_default(&kwargs, heap, "tvos_sdk_version", Some("0.0")),
        tvos_minimum_os_version: kwarg_or_default(
            &kwargs,
            heap,
            "tvos_minimum_os_version",
            Some("0.0"),
        ),
        macos_sdk_version: kwarg_or_default(&kwargs, heap, "macos_sdk_version", Some("0.0")),
        macos_minimum_os_version: kwarg_or_default(
            &kwargs,
            heap,
            "macos_minimum_os_version",
            Some("0.0"),
        ),
        availability: kwarg_or_default(&kwargs, heap, "availability", Some("UNKNOWN")),
        include_xcode_execution_info,
    }
}

fn platform_name<'v>(platform: Value<'v>, heap: Heap<'v>) -> Option<&'v str> {
    platform
        .get_attr("name", heap)
        .ok()
        .flatten()
        .and_then(|name| name.unpack_str())
}

#[starlark_module]
fn bazel_xcode_version_config_methods(builder: &mut MethodsBuilder) {
    fn xcode_version<'v>(
        #[starlark(this)] this: &BazelXcodeVersionConfig<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(match value_str(this.xcode_version.to_value()) {
            Some("") | None => Value::new_none(),
            Some(_) => this.xcode_version.to_value(),
        })
    }

    fn minimum_os_for_platform_type<'v>(
        #[starlark(this)] this: &BazelXcodeVersionConfig<'v>,
        platform_type: Value<'v>,
    ) -> starlark::Result<Value<'v>> {
        let Some(platform_type) = platform_type.unpack_str() else {
            return Err(bazel_apple_error(format!(
                "Unhandled platform type: {}",
                platform_type.to_repr()
            )));
        };
        match platform_type {
            "ios" | "catalyst" => Ok(this.ios_minimum_os_version.to_value()),
            "macos" => Ok(this.macos_minimum_os_version.to_value()),
            "tvos" => Ok(this.tvos_minimum_os_version.to_value()),
            "visionos" => Ok(this.visionos_minimum_os_version.to_value()),
            "watchos" => Ok(this.watchos_minimum_os_version.to_value()),
            other => Err(bazel_apple_error(format!(
                "Unhandled platform type: {other}"
            ))),
        }
    }

    fn sdk_version_for_platform<'v>(
        #[starlark(this)] this: &BazelXcodeVersionConfig<'v>,
        platform: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let Some(name) = platform_name(platform, heap) else {
            return Err(bazel_apple_error(format!(
                "Unhandled platform: {}",
                platform.to_repr()
            )));
        };
        match name {
            "ios_device" | "ios_simulator" => Ok(this.ios_sdk_version.to_value()),
            "macos" | "catalyst" => Ok(this.macos_sdk_version.to_value()),
            "tvos_device" | "tvos_simulator" => Ok(this.tvos_sdk_version.to_value()),
            "visionos_device" | "visionos_simulator" => Ok(this.visionos_sdk_version.to_value()),
            "watchos_device" | "watchos_simulator" => Ok(this.watchos_sdk_version.to_value()),
            other => Err(bazel_apple_error(format!("Unhandled platform: {other}"))),
        }
    }

    fn availability<'v>(
        #[starlark(this)] this: &BazelXcodeVersionConfig<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let availability = value_str(this.availability.to_value()).unwrap_or("UNKNOWN");
        Ok(heap
            .alloc_str(&availability.to_ascii_lowercase())
            .to_value())
    }

    fn execution_info<'v>(
        #[starlark(this)] this: &BazelXcodeVersionConfig<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let mut entries = vec![
            (
                heap.alloc_str("requires-darwin").to_value(),
                heap.alloc_str("").to_value(),
            ),
            (
                heap.alloc_str("supports-xcode-requirements-set").to_value(),
                heap.alloc_str("").to_value(),
            ),
        ];
        match value_str(this.availability.to_value()).unwrap_or("UNKNOWN") {
            "LOCAL" | "local" => entries.push((
                heap.alloc_str("no-remote").to_value(),
                heap.alloc_str("").to_value(),
            )),
            "REMOTE" | "remote" => entries.push((
                heap.alloc_str("no-local").to_value(),
                heap.alloc_str("").to_value(),
            )),
            _ => {}
        }
        if this.include_xcode_execution_info {
            if let Some(xcode_version) = value_str(this.xcode_version.to_value()) {
                if !xcode_version.is_empty() {
                    entries.push((
                        heap.alloc_str(&format!("requires-xcode:{xcode_version}"))
                            .to_value(),
                        heap.alloc_str("").to_value(),
                    ));
                }
            }
        }
        Ok(heap.alloc(AllocDict(entries)))
    }
}

#[starlark_module]
fn bazel_apple_common_module(builder: &mut GlobalsBuilder) {
    fn apple_toolchain() -> starlark::Result<BazelAppleToolchain> {
        Ok(BazelAppleToolchain)
    }

    fn apple_host_system_env<'v>(
        _xcode_config: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn target_apple_env<'v>(
        _xcode_config: Value<'v>,
        _platform: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(AllocDict::EMPTY))
    }

    fn dotted_version(version: &str) -> starlark::Result<String> {
        Ok(version.to_owned())
    }
}

pub(crate) fn register_bazel_apple_common(builder: &mut GlobalsBuilder) {
    builder.namespace("apple_common", |apple| {
        bazel_apple_common_module(apple);
        apple.set("XcodeProperties", NoneType);
        apple.set(
            "XcodeVersionConfig",
            BazelAppleProviderCallable::new("XcodeVersionConfig"),
        );
        apple.set("Objc", BazelAppleProviderCallable::new("ObjcInfo"));
        apple.set(
            "new_objc_provider",
            BazelAppleProviderCallable::new("ObjcInfo"),
        );

        apple.namespace("platform_type", |platform_type| {
            platform_type.set("catalyst", "catalyst");
            platform_type.set("ios", "ios");
            platform_type.set("macos", "macos");
            platform_type.set("tvos", "tvos");
            platform_type.set("visionos", "visionos");
            platform_type.set("watchos", "watchos");
        });

        apple.namespace("platform", |platform| {
            platform.set(
                "ios_device",
                apple_platform(platform, "ios_device", "ios", true, "iPhoneOS"),
            );
            platform.set(
                "ios_simulator",
                apple_platform(platform, "ios_simulator", "ios", false, "iPhoneSimulator"),
            );
            platform.set(
                "macos",
                apple_platform(platform, "macos", "macos", true, "MacOSX"),
            );
            platform.set(
                "catalyst",
                apple_platform(platform, "catalyst", "catalyst", false, "MacOSX"),
            );
            platform.set(
                "tvos_device",
                apple_platform(platform, "tvos_device", "tvos", true, "AppleTVOS"),
            );
            platform.set(
                "tvos_simulator",
                apple_platform(
                    platform,
                    "tvos_simulator",
                    "tvos",
                    false,
                    "AppleTVSimulator",
                ),
            );
            platform.set(
                "visionos_device",
                apple_platform(platform, "visionos_device", "visionos", true, "XROS"),
            );
            platform.set(
                "visionos_simulator",
                apple_platform(
                    platform,
                    "visionos_simulator",
                    "visionos",
                    false,
                    "XRSimulator",
                ),
            );
            platform.set(
                "watchos_device",
                apple_platform(platform, "watchos_device", "watchos", true, "WatchOS"),
            );
            platform.set(
                "watchos_simulator",
                apple_platform(
                    platform,
                    "watchos_simulator",
                    "watchos",
                    false,
                    "WatchSimulator",
                ),
            );
        });
    });
}

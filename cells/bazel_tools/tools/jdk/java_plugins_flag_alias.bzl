# Copyright 2021 The Bazel Authors. All rights reserved.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#    http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Aliases the value of --plugins as a JavaPluginInfo provider."""

load(
    "@rules_java//java/private:java_info.bzl",
    "JavaPluginInfo",
    "merge_plugin_info_without_outputs",
)

def _impl(ctx):
    return [
        merge_plugin_info_without_outputs([
            plugin[JavaPluginInfo]
            for plugin in ctx.attr._java_plugins
            if JavaPluginInfo in plugin
        ]),
    ]

java_plugins_flag_alias = rule(
    implementation = _impl,
    attrs = {
        "_java_plugins": attr.label_list(
            cfg = "exec",
            default = [],
            providers = [JavaPluginInfo],
        ),
    },
)

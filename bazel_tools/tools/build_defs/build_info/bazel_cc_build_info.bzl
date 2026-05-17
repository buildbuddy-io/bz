# Copyright 2023 The Bazel Authors. All rights reserved.
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

"""A rule for getting transliterated build info files for C++."""

def _impl(ctx):
    redacted = ctx.actions.write("redacted_file.h", "", has_content_based_path = False)
    non_volatile = ctx.actions.write("non_volatile_file.h", "", has_content_based_path = False)
    volatile = ctx.actions.write("volatile_file.h", "", has_content_based_path = False)
    return OutputGroupInfo(
        non_redacted_build_info_files = depset([non_volatile, volatile]),
        redacted_build_info_files = depset([redacted]),
    )

bazel_cc_build_info = rule(
    implementation = _impl,
    attrs = {},
)

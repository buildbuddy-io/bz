# Copyright 2018 The Bazel Authors. All rights reserved.
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
"""Constants for action names used for C++ rules."""

C_COMPILE_ACTION_NAME = "c-compile"
CPP_COMPILE_ACTION_NAME = "c++-compile"
LINKSTAMP_COMPILE_ACTION_NAME = "linkstamp-compile"
CC_FLAGS_MAKE_VARIABLE_ACTION_NAME = "cc-flags-make-variable"
CPP_MODULE_CODEGEN_ACTION_NAME = "c++-module-codegen"
CPP_HEADER_PARSING_ACTION_NAME = "c++-header-parsing"
CPP_MODULE_DEPS_SCANNING_ACTION_NAME = "c++-module-deps-scanning"
CPP20_MODULE_COMPILE_ACTION_NAME = "c++20-module-compile"
CPP20_MODULE_CODEGEN_ACTION_NAME = "c++20-module-codegen"
CPP_MODULE_COMPILE_ACTION_NAME = "c++-module-compile"
ASSEMBLE_ACTION_NAME = "assemble"
PREPROCESS_ASSEMBLE_ACTION_NAME = "preprocess-assemble"
LLVM_COV = "llvm-cov"
LTO_INDEXING_ACTION_NAME = "lto-indexing"
LTO_INDEX_FOR_EXECUTABLE_ACTION_NAME = "lto-index-for-executable"
LTO_INDEX_FOR_DYNAMIC_LIBRARY_ACTION_NAME = "lto-index-for-dynamic-library"
LTO_INDEX_FOR_NODEPS_DYNAMIC_LIBRARY_ACTION_NAME = "lto-index-for-nodeps-dynamic-library"
LTO_BACKEND_ACTION_NAME = "lto-backend"
CPP_LINK_EXECUTABLE_ACTION_NAME = "c++-link-executable"
CPP_LINK_DYNAMIC_LIBRARY_ACTION_NAME = "c++-link-dynamic-library"
CPP_LINK_NODEPS_DYNAMIC_LIBRARY_ACTION_NAME = "c++-link-nodeps-dynamic-library"
CPP_LINK_STATIC_LIBRARY_ACTION_NAME = "c++-link-static-library"
STRIP_ACTION_NAME = "strip"
OBJC_COMPILE_ACTION_NAME = "objc-compile"
OBJCPP_COMPILE_ACTION_NAME = "objc++-compile"
OBJC_EXECUTABLE_ACTION_NAME = "objc-executable"
OBJC_FULLY_LINK_ACTION_NAME = "objc-fully-link"
CLIF_MATCH_ACTION_NAME = "clif-match"
OBJ_COPY_ACTION_NAME = "objcopy_embed_data"
VALIDATE_STATIC_LIBRARY = "validate-static-library"

ACTION_NAMES = struct(
    c_compile = C_COMPILE_ACTION_NAME,
    cpp_compile = CPP_COMPILE_ACTION_NAME,
    linkstamp_compile = LINKSTAMP_COMPILE_ACTION_NAME,
    cc_flags_make_variable = CC_FLAGS_MAKE_VARIABLE_ACTION_NAME,
    cpp_module_codegen = CPP_MODULE_CODEGEN_ACTION_NAME,
    cpp_header_parsing = CPP_HEADER_PARSING_ACTION_NAME,
    cpp_module_deps_scanning = CPP_MODULE_DEPS_SCANNING_ACTION_NAME,
    cpp20_module_compile = CPP20_MODULE_COMPILE_ACTION_NAME,
    cpp20_module_codegen = CPP20_MODULE_CODEGEN_ACTION_NAME,
    cpp_module_compile = CPP_MODULE_COMPILE_ACTION_NAME,
    assemble = ASSEMBLE_ACTION_NAME,
    preprocess_assemble = PREPROCESS_ASSEMBLE_ACTION_NAME,
    llvm_cov = LLVM_COV,
    lto_indexing = LTO_INDEXING_ACTION_NAME,
    lto_backend = LTO_BACKEND_ACTION_NAME,
    lto_index_for_executable = LTO_INDEX_FOR_EXECUTABLE_ACTION_NAME,
    lto_index_for_dynamic_library = LTO_INDEX_FOR_DYNAMIC_LIBRARY_ACTION_NAME,
    lto_index_for_nodeps_dynamic_library = LTO_INDEX_FOR_NODEPS_DYNAMIC_LIBRARY_ACTION_NAME,
    cpp_link_executable = CPP_LINK_EXECUTABLE_ACTION_NAME,
    cpp_link_dynamic_library = CPP_LINK_DYNAMIC_LIBRARY_ACTION_NAME,
    cpp_link_nodeps_dynamic_library = CPP_LINK_NODEPS_DYNAMIC_LIBRARY_ACTION_NAME,
    cpp_link_static_library = CPP_LINK_STATIC_LIBRARY_ACTION_NAME,
    strip = STRIP_ACTION_NAME,
    objc_compile = OBJC_COMPILE_ACTION_NAME,
    objc_executable = OBJC_EXECUTABLE_ACTION_NAME,
    objc_fully_link = OBJC_FULLY_LINK_ACTION_NAME,
    objcpp_compile = OBJCPP_COMPILE_ACTION_NAME,
    clif_match = CLIF_MATCH_ACTION_NAME,
    objcopy_embed_data = OBJ_COPY_ACTION_NAME,
    validate_static_library = VALIDATE_STATIC_LIBRARY,
)

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(error_generic_member_access)]
#![feature(decl_macro)]
#![feature(never_type)]
#![feature(pattern)]
#![feature(box_patterns)]
#![feature(impl_trait_in_assoc_type)]
#![feature(io_error_more)]
#![feature(once_cell_try)]
#![feature(try_blocks)]
#![feature(used_with_arg)]
#![feature(try_trait_v2)]

// Re-export these because we don't want to make people add a dependency on this crate everywhere
pub use bz_env::env;
pub use bz_env::soft_error as error;

mod ascii_char_set;
pub mod async_once_cell;
pub mod build_file_path;
pub mod bxl;
pub mod bzl;
pub mod category;
pub mod cells;
pub mod ci;
pub mod client_only;
pub mod configuration;
pub mod content_hash;
pub mod deferred;
pub mod directory_digest;
pub mod event;
pub mod execution_types;
pub mod faster_directories;
pub mod fs;
pub mod global_cfg_options;
pub use bz_fs::io_counters;
pub mod logging;
pub mod package;
pub mod pattern;
pub mod plugins;
pub mod provider;
pub mod quick_debug_event;
pub mod rollout_percentage;
pub mod target;
pub mod target_aliases;
pub mod unsafe_send_future;

// Re-export macros from bz_env so they're available at the crate root
pub use bz_env::env::bz_env;
pub use bz_env::env::bz_env_name;
// Re-export these macros at the crate root so they work like before when error was #[macro_use]
#[doc(inline)]
pub use bz_env::soft_error::soft_error;
#[doc(inline)]
pub use bz_env::soft_error::tag_error;
#[doc(inline)]
pub use bz_env::soft_error::tag_result;

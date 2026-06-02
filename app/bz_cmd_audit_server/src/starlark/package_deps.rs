/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::io::Write;

use bz_cli_proto::ClientContext;
use bz_cmd_audit_client::starlark::package_deps::StarlarkPackageDepsCommand;
use bz_common::dice::cells::HasCellResolver;
use bz_core::bzl::ImportPath;
use bz_core::pattern::parse_package::parse_package;
use bz_error::bz_error;
use bz_hash::StdBuckHashSet;
use bz_interpreter::file_loader::LoadedModule;
use bz_interpreter::load_module::INTERPRETER_CALCULATION_IMPL;
use bz_interpreter::paths::module::StarlarkModulePath;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::ctx::ServerCommandDiceContext;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;

pub(crate) async fn server_execute(
    command: &StarlarkPackageDepsCommand,
    server_ctx: &dyn ServerCommandContextTrait,
    mut stdout: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
    _client_ctx: ClientContext,
) -> bz_error::Result<()> {
    server_ctx
        .with_dice_ctx(|server_ctx, mut dice_ctx| async move {
            let cell_resolver = dice_ctx.get_cell_resolver().await?;
            let cwd = server_ctx.working_dir();
            let current_cell_path = cell_resolver.get_cell_path(cwd);
            let cell_alias_resolver = dice_ctx
                .get_cell_alias_resolver(current_cell_path.cell())
                .await?;

            let package = parse_package(&command.package, &cell_alias_resolver)?;

            let module_deps = INTERPRETER_CALCULATION_IMPL
                .get()?
                .get_module_deps(&mut dice_ctx, package)
                .await?;

            let mut stdout = stdout.as_writer();

            struct Printer {
                first: bool,
                visited: StdBuckHashSet<ImportPath>,
            }

            impl Printer {
                fn print_module_and_deps(
                    &mut self,
                    module: &LoadedModule,
                    stdout: &mut dyn Write,
                ) -> bz_error::Result<()> {
                    let path = match module.path() {
                        StarlarkModulePath::LoadFile(path)
                        | StarlarkModulePath::JsonFile(path)
                        | StarlarkModulePath::TomlFile(path) => path,
                        StarlarkModulePath::BxlFile(_) => {
                            return Err(bz_error!(bz_error::ErrorTag::Tier0, "bxl be here"));
                        }
                    };

                    if !self.visited.insert(path.clone()) {
                        return Ok(());
                    }

                    for import in module.loaded_modules().map.values() {
                        self.print_module_and_deps(import, stdout)?;
                    }

                    if !self.first {
                        writeln!(stdout)?;
                        writeln!(stdout)?;
                    }
                    self.first = false;

                    writeln!(stdout, "# {path}")?;
                    writeln!(stdout)?;
                    write!(stdout, "{}", module.env().dump_debug())?;

                    Ok(())
                }
            }

            let mut printer = Printer {
                first: true,
                visited: StdBuckHashSet::default(),
            };

            for module in module_deps.0.into_iter() {
                printer.print_module_and_deps(&module, &mut stdout)?;
            }

            Ok(())
        })
        .await
}

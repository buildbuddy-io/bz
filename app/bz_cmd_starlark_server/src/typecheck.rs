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

use async_recursion::async_recursion;
use async_trait::async_trait;
use bz_cli_proto::ClientContext;
use bz_cmd_starlark_client::typecheck::StarlarkTypecheckCommand;
use bz_common::dice::cells::HasCellResolver;
use bz_common::dice::data::HasIoProvider;
use bz_common::io::IoProvider;
use bz_core::cells::CellResolver;
use bz_core::cells::name::CellName;
use bz_error::bz_error;
use bz_error::internal_error;
use bz_hash::StdBuckHashMap;
use bz_interpreter::file_type::StarlarkFileType;
use bz_interpreter::paths::module::OwnedStarlarkModulePath;
use bz_interpreter::paths::path::OwnedStarlarkPath;
use bz_interpreter_for_build::interpreter::dice_calculation_delegate::HasCalculationDelegate;
use bz_server_ctx::ctx::ServerCommandContextTrait;
use bz_server_ctx::ctx::ServerCommandDiceContext;
use bz_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use dice::DiceTransaction;
use dupe::Dupe;
use starlark::environment::Globals;
use starlark::typing::AstModuleTypecheck;
use starlark::typing::Interface;

use crate::StarlarkServerSubcommand;
use crate::util::environment::Environment;
use crate::util::paths::starlark_files;

struct Cache<'a> {
    // Things we have access to get information
    dice: &'a DiceTransaction,
    io: &'a dyn IoProvider,
    cell_resolver: &'a CellResolver,
    // Things we have access to write information
    stdout: &'a mut (dyn Write + Send + Sync),
    stderr: &'a mut (dyn Write + Send + Sync),
    // Our accumulated state
    oracle: StdBuckHashMap<(CellName, StarlarkFileType), Globals>,
    cache: StdBuckHashMap<OwnedStarlarkModulePath, Interface>,
}

impl Cache<'_> {
    async fn typecheck(&mut self, path: OwnedStarlarkPath) -> bz_error::Result<()> {
        self.run(path).await?;
        Ok(())
    }

    async fn get_oracle(
        &mut self,
        cell: CellName,
        path_type: StarlarkFileType,
    ) -> bz_error::Result<Globals> {
        match self.oracle.get(&(cell, path_type)) {
            Some(g) => Ok(g.dupe()),
            None => {
                let globals = Environment::new(cell, path_type, &mut self.dice.clone())
                    .await?
                    .globals;
                self.oracle.insert((cell, path_type), globals.dupe());
                Ok(globals)
            }
        }
    }

    async fn get(&mut self, path: OwnedStarlarkModulePath) -> bz_error::Result<Interface> {
        match self.cache.get(&path) {
            Some(x) => Ok(x.dupe()),
            None => {
                let res = self.run(path.clone().into_starlark_path()).await?;
                self.cache.insert(path, res.dupe());
                Ok(res)
            }
        }
    }

    #[async_recursion]
    async fn run(&mut self, path: OwnedStarlarkPath) -> bz_error::Result<Interface> {
        let path_ref = path.borrow();
        writeln!(self.stderr, "Type checking: {path_ref}")?;
        let proj_path = self
            .cell_resolver
            .resolve_path(path_ref.path().as_ref().as_ref())?;
        let path_str = proj_path.to_string();
        let src = self
            .io
            .read_file_if_exists(proj_path)
            .await?
            .ok_or_else(|| internal_error!("File not found: `{path_str}`"))?;

        let mut dice = self.dice.clone();
        let interp = dice
            .get_interpreter_calculator(OwnedStarlarkPath::new(path_ref))
            .await?;

        let ast = interp.prepare_eval_with_content(path_ref, src)??.ast;
        let mut loads = StdBuckHashMap::default();
        for x in ast.loads() {
            let y = interp.resolve_load(path_ref, x.module_id).await?;
            let interface = self.get(y).await?;
            loads.insert(x.module_id.to_owned(), interface);
        }
        let globals = self
            .get_oracle(path_ref.cell(), path_ref.file_type())
            .await?;
        let (errors, bindings, interface, approxiomations) = ast.typecheck(&globals, &loads);

        if !approxiomations.is_empty() {
            writeln!(self.stderr, "\n\nAPPROXIMATIONS:")?;
            for x in approxiomations {
                writeln!(self.stderr, "{x}")?;
            }
        }

        writeln!(self.stderr, "\n\nBINDINGS:\n{bindings}")?;

        let errors_count = errors.len();
        if errors_count == 0 {
            Ok(interface)
        } else {
            writeln!(self.stdout, "\n\nERRORS:")?;
            for x in errors {
                writeln!(self.stdout, "{x}")?;
            }
            Err(bz_error!(
                bz_error::ErrorTag::Input,
                "Detected {errors_count} errors"
            ))
        }
    }
}

#[async_trait]
impl StarlarkServerSubcommand for StarlarkTypecheckCommand {
    async fn server_execute(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        mut stdout: PartialResultDispatcher<bz_cli_proto::StdoutBytes>,
        _client_ctx: ClientContext,
    ) -> bz_error::Result<()> {
        Ok(server_ctx
            .with_dice_ctx(|server_ctx, mut dice| async move {
                let cell_resolver = &dice.get_cell_resolver().await?;
                let io = &dice.global_data().get_io_provider();

                let files =
                    starlark_files(&mut dice, &self.paths, server_ctx, cell_resolver, &**io)
                        .await?;
                let mut stdout = stdout.as_writer();
                let mut stderr = server_ctx.stderr()?;
                let mut cache = Cache {
                    dice: &dice,
                    io: &**io,
                    cell_resolver,
                    stdout: &mut stdout,
                    stderr: &mut stderr,
                    oracle: StdBuckHashMap::default(),
                    cache: StdBuckHashMap::default(),
                };
                for file in files {
                    cache.typecheck(file).await?;
                }
                let file_count = cache.cache.len();
                writeln!(stderr, "Found no type errors in {file_count} files")?;
                Ok(())
            })
            .await?)
    }
}

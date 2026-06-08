/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::borrow::Cow;
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_build_api::actions::Action;
use bz_build_api::actions::ActionExecutionCtx;
use bz_build_api::actions::UnregisteredAction;
use bz_build_api::actions::execute::action_executor::ActionExecutionKind;
use bz_build_api::actions::execute::action_executor::ActionExecutionMetadata;
use bz_build_api::actions::execute::action_executor::ActionOutputs;
use bz_build_api::actions::execute::error::ExecuteError;
use bz_build_api::artifact_groups::ArtifactGroup;
use bz_build_signals::env::WaitingData;
use bz_common::cas_digest::RawDigest;
use bz_common::file_ops::metadata::FileDigest;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_common::io::trace::TracingIoProvider;
use bz_core::category::CategoryRef;
use bz_core::fs::buck_out_path::BuildArtifactPath;
use bz_error::BuckErrorContext;
use bz_error::ErrorTag;
use bz_error::conversion::from_any_with_tag;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::DigestConfig;
use bz_execute::execute::command_executor::ActionExecutionTimingData;
use bz_execute::materialize::http::Checksum;
use bz_execute::materialize::http::http_download;
use bz_execute::materialize::http::http_head;
use bz_execute::materialize::materializer::DeclareArtifactPayload;
use bz_execute::materialize::materializer::HttpDownloadInfo;
use bz_hash::BuckIndexSet;
use bz_http::HttpClient;
use dupe::Dupe;
use pagable::Pagable;
use starlark::values::OwnedFrozenValue;

use crate::actions::impls::offline;

#[derive(Debug, bz_error::Error)]
#[buck2(tag = Input)]
enum DownloadFileActionError {
    #[error("Exactly one output file must be specified for a download file action, got {0}")]
    WrongNumberOfOutputs(usize),
    #[error(
        "Downloads using content-based path {0} must supply metadata (usually in the form of a sha1)!"
    )]
    ContentBasedPathWithoutMetadata(BuildArtifactPath),
}

#[derive(Debug, Allocative, Pagable)]
pub(crate) struct UnregisteredDownloadFileAction {
    checksum: Checksum,
    size_bytes: Option<u64>,
    url: Arc<str>,
    vpnless_url: Option<Arc<str>>,
    is_executable: bool,
}

impl UnregisteredDownloadFileAction {
    pub(crate) fn new(
        checksum: Checksum,
        size_bytes: Option<u64>,
        url: Arc<str>,
        vpnless_url: Option<Arc<str>>,
        is_executable: bool,
    ) -> Self {
        Self {
            checksum,
            size_bytes,
            url,
            vpnless_url,
            is_executable,
        }
    }
}

impl UnregisteredAction for UnregisteredDownloadFileAction {
    fn register(
        self: Box<Self>,
        outputs: BuckIndexSet<BuildArtifact>,
        _starlark_data: Option<OwnedFrozenValue>,
        _error_handler: Option<OwnedFrozenValue>,
    ) -> bz_error::Result<Box<dyn Action>> {
        Ok(Box::new(DownloadFileAction::new(outputs, *self)?))
    }
}

#[derive(Debug, Allocative, Pagable)]
struct DownloadFileAction {
    outputs: Box<[BuildArtifact]>,
    inner: UnregisteredDownloadFileAction,
}

impl DownloadFileAction {
    fn new(
        outputs: BuckIndexSet<BuildArtifact>,
        inner: UnregisteredDownloadFileAction,
    ) -> bz_error::Result<Self> {
        if outputs.len() != 1 {
            Err(DownloadFileActionError::WrongNumberOfOutputs(outputs.len()).into())
        } else {
            Ok(Self {
                outputs: outputs.into_iter().collect(),
                inner,
            })
        }
    }

    fn output(&self) -> &BuildArtifact {
        self.outputs
            .iter()
            .next()
            .expect("a single artifact by construction")
    }

    fn url(&self, client: &HttpClient) -> &Arc<str> {
        if client.supports_vpnless() {
            self.inner.vpnless_url.as_ref().unwrap_or(&self.inner.url)
        } else {
            &self.inner.url
        }
    }

    /// Try to produce a FileMetadata without downloading the file.
    async fn declared_metadata(
        &self,
        client: &HttpClient,
        digest_config: DigestConfig,
    ) -> bz_error::Result<Option<FileMetadata>> {
        let digest = if digest_config.cas_digest_config().allows_sha1() {
            self.inner
                .checksum
                .sha1()
                .and_then(|sha1| RawDigest::parse_sha1(sha1.as_bytes()).ok())
        } else if digest_config.cas_digest_config().allows_sha256() {
            self.inner
                .checksum
                .sha256()
                .and_then(|sha256| RawDigest::parse_sha256(sha256.as_bytes()).ok())
        } else {
            None
        };

        let digest = match digest {
            Some(digest) => digest,
            None => return Ok(None),
        };

        let size = match self.inner.size_bytes {
            Some(s) => Some(s),
            None => {
                let url = self.url(client);
                let head = http_head(client, url)
                    .await
                    .map_err(|e| e.tag([ErrorTag::DownloadFileHeadRequest]))?;

                head.headers()
                    .get(http::header::CONTENT_LENGTH)
                    .map(|content_length| {
                        let content_length = content_length
                            .to_str()
                            .map_err(|e| from_any_with_tag(e, bz_error::ErrorTag::Http))
                            .buck_error_context("Header is not valid utf-8")?;
                        let content_length_number =
                            content_length.parse().with_buck_error_context(|| {
                                format!("Header is not a number: `{content_length}`")
                            })?;
                        bz_error::Ok(content_length_number)
                    })
                    .transpose()
                    .with_buck_error_context(|| {
                        format!(
                            "Request to `{}` returned an invalid `{}` header",
                            url,
                            http::header::CONTENT_LENGTH
                        )
                    })?
            }
        };

        match size {
            Some(size) => {
                let digest = TrackedFileDigest::new(
                    FileDigest::new(digest, size),
                    digest_config.cas_digest_config(),
                );
                Ok(Some(FileMetadata {
                    digest,
                    is_executable: self.inner.is_executable,
                }))
            }
            None => Ok(None),
        }
    }

    /// Execute this action for offline builds (e.g. no network).
    async fn execute_for_offline(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
    ) -> bz_error::Result<(ActionOutputs, ActionExecutionMetadata)> {
        let outputs = offline::declare_copy_from_offline_cache(ctx, &[self.output()]).await?;

        Ok((
            outputs,
            ActionExecutionMetadata {
                execution_kind: ActionExecutionKind::Simple,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data: WaitingData::new(),
                remote_cache_origin: None,
            },
        ))
    }
}

#[async_trait]
impl Action for DownloadFileAction {
    fn kind(&self) -> bz_data::ActionKind {
        bz_data::ActionKind::DownloadFile
    }

    fn inputs(&self) -> bz_error::Result<Cow<'_, [ArtifactGroup]>> {
        Ok(Cow::Borrowed(&[]))
    }

    fn outputs(&self) -> Cow<'_, [BuildArtifact]> {
        Cow::Borrowed(&self.outputs)
    }

    fn first_output(&self) -> &BuildArtifact {
        self.output()
    }

    fn category(&self) -> CategoryRef<'_> {
        CategoryRef::unchecked_new("download_file")
    }

    fn identifier(&self) -> Option<&str> {
        self.outputs
            .iter()
            .next()
            .map(|o| o.get_path().path().as_str())
    }

    async fn execute(
        &self,
        ctx: &mut dyn ActionExecutionCtx,
        waiting_data: WaitingData,
    ) -> Result<(ActionOutputs, ActionExecutionMetadata), ExecuteError> {
        // Early return - if this path exists, it's because we're running in a
        // special offline mode where the HEAD request below will likely fail.
        // Shortcut and just return this path as the action output.
        //
        // This mostly looks like a "copy" action.
        if ctx.run_action_knobs().use_network_action_output_cache {
            return self.execute_for_offline(ctx).await.map_err(Into::into);
        }

        let client = ctx.http_client();
        let url = self.url(&client);

        let (value, execution_kind) = {
            match self.declared_metadata(&client, ctx.digest_config()).await? {
                Some(metadata) => {
                    let artifact_fs = ctx.fs();
                    let value = ArtifactValue::file(metadata.dupe());
                    let rel_path = artifact_fs.resolve_build(
                        self.output().get_path(),
                        if self.output().get_path().is_content_based_path() {
                            Some(value.content_based_path_hash())
                        } else {
                            None
                        }
                        .as_ref(),
                    )?;

                    let configuration_path = ctx
                        .materializer()
                        .maybe_eager_configuration_path(ctx.fs(), self.output().get_path())?;

                    // Fast path: download later via the materializer.
                    ctx.materializer()
                        .declare_http(
                            rel_path,
                            HttpDownloadInfo {
                                url: url.dupe(),
                                checksum: self.inner.checksum.dupe(),
                                metadata,
                                owner: ctx.target().owner().dupe(),
                            },
                            configuration_path,
                        )
                        .await?;

                    (value, ActionExecutionKind::Deferred)
                }
                None => {
                    if self.output().get_path().is_content_based_path() {
                        return Err(ExecuteError::Error {
                            error: DownloadFileActionError::ContentBasedPathWithoutMetadata(
                                self.output().get_path().dupe(),
                            )
                            .into(),
                        });
                    }

                    ctx.cleanup_outputs().await?;

                    let artifact_fs = ctx.fs();
                    let project_fs = artifact_fs.fs();

                    let rel_path = artifact_fs.resolve_build(self.output().get_path(), None)?;

                    // Slow path: download now.
                    let digest = http_download(
                        &client,
                        project_fs,
                        ctx.digest_config(),
                        &rel_path,
                        url,
                        &self.inner.checksum,
                        self.inner.is_executable,
                    )
                    .await?;

                    let metadata = FileMetadata {
                        digest,
                        is_executable: self.inner.is_executable,
                    };
                    ctx.materializer()
                        .declare_existing(vec![DeclareArtifactPayload {
                            path: rel_path,
                            artifact: ArtifactValue::file(metadata.dupe()),
                            configuration_path: None,
                        }])
                        .await?;

                    (ArtifactValue::file(metadata), ActionExecutionKind::Simple)
                }
            }
        };

        // If we're tracing I/O, get the materializer to copy to the offline cache
        // so we can include it in the offline archive manifest later.
        let io_provider = ctx.io_provider();
        if let Some(tracer) = TracingIoProvider::from_io(&*io_provider) {
            let offline_cache_path =
                offline::declare_copy_to_offline_output_cache(ctx, self.output(), value.dupe())
                    .await?;
            tracer.add_buck_out_entry(offline_cache_path);
        }

        Ok((
            ActionOutputs::from_single(self.output().get_path().dupe(), value),
            ActionExecutionMetadata {
                execution_kind,
                timing: ActionExecutionTimingData::default(),
                input_files_bytes: None,
                waiting_data,
                remote_cache_origin: None,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    // TODO: This needs proper tests, but right now it's kind of a pain to get the
    //       action framework up and running to test actions
    #[test]
    fn downloads_file() {}
}

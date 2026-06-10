/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use bz_error::BuckErrorContext as _;
use bz_error::internal_error;
use bz_grpc::ServerHandle;
use bz_grpc::make_channel;
use bz_grpc::spawn_oneshot;
use bz_grpc::to_tonic;
use bz_test_proto::BazelTestSpecRequest;
use bz_test_proto::Empty;
use bz_test_proto::ExternalRunnerSpecRequest;
use bz_test_proto::UnstableHeapDumpRequest;
use bz_test_proto::UnstableHeapDumpResponse;
use bz_test_proto::test_executor_client;
use bz_test_proto::test_executor_server;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tonic::transport::Channel;

use crate::data::BazelTestSpec;
use crate::data::ExternalRunnerSpec;
use crate::protocol::TestExecutor;

pub struct TestExecutorClient {
    client: test_executor_client::TestExecutorClient<Channel>,
}

impl TestExecutorClient {
    pub async fn new<T>(io: T) -> bz_error::Result<Self>
    where
        T: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    {
        let channel = make_channel(io, "executor").await?;

        Ok(Self {
            client: test_executor_client::TestExecutorClient::new(channel)
                .max_encoding_message_size(usize::MAX)
                .max_decoding_message_size(usize::MAX),
        })
    }
}

#[async_trait::async_trait]
impl TestExecutor for TestExecutorClient {
    async fn external_runner_spec(&self, s: ExternalRunnerSpec) -> bz_error::Result<()> {
        self.client
            .clone()
            .external_runner_spec(ExternalRunnerSpecRequest {
                test_spec: Some(s.try_into().buck_error_context("Invalid `test_spec`")?),
            })
            .await?;

        Ok(())
    }

    async fn bazel_test_spec(&self, s: BazelTestSpec) -> bz_error::Result<()> {
        self.client
            .clone()
            .bazel_test_spec(BazelTestSpecRequest {
                test_spec: Some(s.try_into().buck_error_context("Invalid `test_spec`")?),
            })
            .await?;

        Ok(())
    }

    async fn end_of_test_requests(&self) -> bz_error::Result<()> {
        self.client.clone().end_of_test_requests(Empty {}).await?;
        Ok(())
    }

    async fn unstable_heap_dump(&self, path: &str) -> bz_error::Result<()> {
        self.client
            .clone()
            .unstable_heap_dump(UnstableHeapDumpRequest {
                destination_path: path.into(),
            })
            .await?;
        Ok(())
    }
}

pub struct Service<T> {
    inner: T,
}

#[async_trait::async_trait]
impl<T> test_executor_server::TestExecutor for Service<T>
where
    T: TestExecutor + Send + Sync + 'static,
{
    async fn external_runner_spec(
        &self,
        request: tonic::Request<ExternalRunnerSpecRequest>,
    ) -> Result<tonic::Response<Empty>, tonic::Status> {
        to_tonic(async move {
            let ExternalRunnerSpecRequest { test_spec } = request.into_inner();

            let test_spec = test_spec
                .ok_or_else(|| internal_error!("Missing `test_spec`"))?
                .try_into()
                .buck_error_context("Invalid `test_spec`")?;

            self.inner
                .external_runner_spec(test_spec)
                .await
                .buck_error_context("Failed to dispatch test_spec")?;

            Ok(Empty {})
        })
        .await
    }

    async fn bazel_test_spec(
        &self,
        request: tonic::Request<BazelTestSpecRequest>,
    ) -> Result<tonic::Response<Empty>, tonic::Status> {
        to_tonic(async move {
            let BazelTestSpecRequest { test_spec } = request.into_inner();

            let test_spec = test_spec
                .ok_or_else(|| internal_error!("Missing `test_spec`"))?
                .try_into()
                .buck_error_context("Invalid `test_spec`")?;

            self.inner
                .bazel_test_spec(test_spec)
                .await
                .buck_error_context("Failed to dispatch bazel test_spec")?;

            Ok(Empty {})
        })
        .await
    }

    async fn end_of_test_requests(
        &self,
        _: tonic::Request<Empty>,
    ) -> Result<tonic::Response<Empty>, tonic::Status> {
        to_tonic(async move {
            self.inner
                .end_of_test_requests()
                .await
                .buck_error_context("Failed to report end-of-tests")?;

            Ok(Empty {})
        })
        .await
    }

    async fn unstable_heap_dump(
        &self,
        req: tonic::Request<UnstableHeapDumpRequest>,
    ) -> Result<tonic::Response<UnstableHeapDumpResponse>, tonic::Status> {
        to_tonic(async move {
            self.inner
                .unstable_heap_dump(&req.into_inner().destination_path)
                .await
                .buck_error_context("Failed to dispatch unstable_heap_dump")?;
            Ok(UnstableHeapDumpResponse {})
        })
        .await
    }
}

pub fn spawn_executor_server<I, E>(io: I, executor: E) -> ServerHandle
where
    I: AsyncRead + AsyncWrite + Send + Unpin + 'static + tonic::transport::server::Connected,
    E: TestExecutor + Send + Sync + 'static,
{
    let router = tonic::transport::Server::builder().add_service(
        test_executor_server::TestExecutorServer::new(Service { inner: executor })
            .max_encoding_message_size(usize::MAX)
            .max_decoding_message_size(usize::MAX),
    );

    spawn_oneshot(io, router)
}

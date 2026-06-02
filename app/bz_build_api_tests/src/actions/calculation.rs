/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use assert_matches::assert_matches;
use async_trait::async_trait;
use bz_analysis::analysis::calculation::AnalysisKey;
use bz_artifact::actions::key::ActionIndex;
use bz_artifact::actions::key::ActionKey;
use bz_artifact::artifact::artifact_type::Artifact;
use bz_artifact::artifact::artifact_type::testing::BuildArtifactTestingExt;
use bz_artifact::artifact::build_artifact::BuildArtifact;
use bz_artifact::artifact::source_artifact::SourceArtifact;
use bz_build_api::actions::Action;
use bz_build_api::actions::RegisteredAction;
use bz_build_api::actions::calculation::ActionCalculation;
use bz_build_api::actions::calculation::command_details;
use bz_build_api::actions::execute::dice_data::CommandExecutorResponse;
use bz_build_api::actions::execute::dice_data::HasCommandExecutor;
use bz_build_api::actions::execute::dice_data::SetCommandExecutor;
use bz_build_api::actions::execute::dice_data::SetInvalidationTrackingConfig;
use bz_build_api::actions::execute::dice_data::SetReClient;
use bz_build_api::actions::execute::dice_data::set_fallback_executor_config;
use bz_build_api::actions::impls::run_action_knobs::RunActionKnobs;
use bz_build_api::actions::registry::RecordedActions;
use bz_build_api::analysis::AnalysisResult;
use bz_build_api::analysis::registry::RecordedAnalysisValues;
use bz_build_api::artifact_groups::ArtifactGroup;
use bz_build_api::artifact_groups::calculation::ArtifactGroupCalculation;
use bz_build_api::build::detailed_aggregated_metrics::dice::SetDetailedAggregatedMetricsEventHandler;
use bz_build_api::build::detailed_aggregated_metrics::dice::SetDetailedAggregatedMetricsEventsHolder;
use bz_build_api::context::SetBuildContextData;
use bz_build_api::keep_going::HasKeepGoing;
use bz_build_api::spawner::BuckSpawner;
use bz_common::dice::cells::SetCellResolver;
use bz_common::dice::data::testing::SetTestingIoProvider;
use bz_common::external_symlink::ExternalSymlink;
use bz_common::file_ops::metadata::FileMetadata;
use bz_common::file_ops::metadata::TrackedFileDigest;
use bz_common::file_ops::testing::TestFileOps;
use bz_common::http::SetHttpClient;
use bz_common::legacy_configs::configs::LegacyBuckConfig;
use bz_common::legacy_configs::dice::inject_legacy_config_for_test;
use bz_configured::nodes::ConfiguredTargetNodeKey;
use bz_core::category::CategoryRef;
use bz_core::cells::CellResolver;
use bz_core::cells::cell_path::CellPath;
use bz_core::cells::cell_root_path::CellRootPathBuf;
use bz_core::cells::name::CellName;
use bz_core::cells::paths::CellRelativePathBuf;
use bz_core::configuration::compatibility::MaybeCompatible;
use bz_core::configuration::compatibility::ResultMaybeCompatible;
use bz_core::configuration::data::ConfigurationData;
use bz_core::deferred::base_deferred_key::BaseDeferredKey;
use bz_core::deferred::key::DeferredHolderKey;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::execution_types::executor_config::CommandExecutorConfig;
use bz_core::fs::artifact_path_resolver::ArtifactFs;
use bz_core::fs::project::ProjectRootTemp;
use bz_core::fs::project_rel_path::ProjectRelativePathBuf;
use bz_core::package::source_path::SourcePath;
use bz_core::target::configured_target_label::ConfiguredTargetLabel;
use bz_core::target::label::label::TargetLabel;
use bz_directory::directory::entry::DirectoryEntry;
use bz_events::dispatch::EventDispatcher;
use bz_events::dispatch::with_dispatcher_async;
use bz_execute::artifact_value::ArtifactValue;
use bz_execute::digest_config::DigestConfig;
use bz_execute::digest_config::SetDigestConfig;
use bz_execute::directory::ActionDirectoryMember;
use bz_execute::execute::action_digest::ActionDigest;
use bz_execute::execute::blocking::SetBlockingExecutor;
use bz_execute::execute::blocking::testing::DummyBlockingExecutor;
use bz_execute::execute::cache_uploader::NoOpCacheUploader;
use bz_execute::execute::kind::CommandExecutionKind;
use bz_execute::execute::output::CommandStdStreams;
use bz_execute::execute::prepared::NoOpCommandOptionalExecutor;
use bz_execute::execute::request::CommandExecutionOutput;
use bz_execute::execute::request::OutputType;
use bz_execute::execute::result::CommandExecutionMetadata;
use bz_execute::execute::result::CommandExecutionReport;
use bz_execute::execute::result::CommandExecutionStatus;
use bz_execute::execute::testing_dry_run::DryRunEntry;
use bz_execute::execute::testing_dry_run::DryRunExecutor;
use bz_execute::materialize::materializer::SetMaterializer;
use bz_execute::materialize::nodisk::NoDiskMaterializer;
use bz_execute::re::manager::UnconfiguredRemoteExecutionClient;
use bz_file_watcher::mergebase::SetMergebase;
use bz_fs::paths::forward_rel_path::ForwardRelativePathBuf;
use bz_hash::buck_indexset;
use bz_http::HttpClientBuilder;
use bz_node::nodes::configured::ConfiguredTargetNode;
use bz_util::time_span::TimeSpan;
use dice::DiceTransaction;
use dice::UserComputationData;
use dice::testing::DiceBuilder;
use dupe::Dupe;
use maplit::btreemap;
use sorted_vector_map::sorted_vector_map;

use crate::actions::testings::SimpleAction;

fn create_test_configured_target_label() -> ConfiguredTargetLabel {
    TargetLabel::testing_parse("cell//pkg:foo").configure(ConfigurationData::testing_new())
}

fn create_test_build_artifact() -> BuildArtifact {
    let configured_target_label = create_test_configured_target_label();
    let deferred_id = ActionIndex::new(0);
    BuildArtifact::testing_new(configured_target_label, "bar.out", deferred_id)
}

fn create_test_source_artifact(package_label: &str, target_name: &str) -> SourceArtifact {
    SourceArtifact::new(SourcePath::testing_new(package_label, target_name))
}

fn registered_action(
    build_artifact: BuildArtifact,
    action: Box<dyn Action>,
) -> Arc<RegisteredAction> {
    let registered_action = RegisteredAction::new(
        build_artifact.key().dupe(),
        action,
        CommandExecutorConfig::testing_local(),
        None,
    );
    Arc::new(registered_action)
}

fn mock_analysis_for_action_resolution(
    mut dice_builder: DiceBuilder,
    action_key: &ActionKey,
    registered_action_arc: Arc<RegisteredAction>,
) -> DiceBuilder {
    let configured_target_label = create_test_configured_target_label();
    let configured_node_key = ConfiguredTargetNodeKey(configured_target_label.dupe());

    assert_eq!(
        &DeferredHolderKey::Base(BaseDeferredKey::TargetLabel(configured_target_label.dupe())),
        action_key.holder_key()
    );

    let mut actions = RecordedActions::new(1);
    actions.insert(action_key.dupe(), registered_action_arc);

    dice_builder = dice_builder.mock_and_return(
        AnalysisKey(configured_target_label.dupe()),
        bz_error::Ok(MaybeCompatible::Compatible(AnalysisResult::new(
            RecordedAnalysisValues::testing_new(
                action_key.holder_key().dupe(),
                Vec::new(),
                actions,
            ),
            None,
            HashMap::new(),
            0,
            0,
            None,
        ))),
    );

    dice_builder.mock_and_return(
        configured_node_key,
        ResultMaybeCompatible::Compatible(ConfiguredTargetNode::testing_new(
            configured_target_label,
            "foo_lib",
            ExecutionPlatformResolution::new_for_testing(None, Vec::new()),
            vec![],
            None,
        )),
    )
}

async fn make_default_dice_state(
    dry_run_tracker: Arc<Mutex<Vec<DryRunEntry>>>,
    temp_fs: &ProjectRootTemp,
    mocks: Vec<Box<dyn FnOnce(DiceBuilder) -> DiceBuilder>>,
) -> bz_error::Result<DiceTransaction> {
    let fs = temp_fs.path().dupe();

    let cell_resolver = CellResolver::testing_with_name_and_path(
        CellName::testing_new("cell"),
        CellRootPathBuf::new(ProjectRelativePathBuf::unchecked_new("cell-path".into())),
    );
    let output_path = ProjectRelativePathBuf::unchecked_new("buck-out/v2".into());

    let mut dice_builder = DiceBuilder::new();
    dice_builder = dice_builder.set_data(|data| {
        data.set_testing_io_provider(temp_fs);
        data.set_digest_config(DigestConfig::testing_default());
        data.set_invalidation_tracking_config(true);
        data.set_detailed_aggregated_metrics_event_handler(None);
    });

    for mock in mocks.into_iter() {
        dice_builder = mock(dice_builder);
    }

    let mut extra = UserComputationData::new();
    extra.set_keep_going(true);
    struct CommandExecutorProvider {
        dry_run_tracker: Arc<Mutex<Vec<DryRunEntry>>>,
    }
    #[async_trait]
    impl HasCommandExecutor for CommandExecutorProvider {
        async fn get_command_executor(
            &self,
            artifact_fs: &ArtifactFs,
            _config: &CommandExecutorConfig,
        ) -> bz_error::Result<CommandExecutorResponse> {
            let executor = Arc::new(DryRunExecutor::new(
                self.dry_run_tracker.dupe(),
                artifact_fs.clone(),
            ));
            Ok(CommandExecutorResponse {
                executor,
                action_cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                remote_dep_file_cache_checker: Arc::new(NoOpCommandOptionalExecutor {}),
                platform: Default::default(),
                cache_uploader: Arc::new(NoOpCacheUploader {}),
                output_trees_download_config:
                    bz_execute::re::output_trees_download_config::OutputTreesDownloadConfig::new(
                        None, true,
                    ),
            })
        }
    }

    set_fallback_executor_config(&mut extra.data, CommandExecutorConfig::testing_local());
    extra.set_command_executor(Box::new(CommandExecutorProvider { dry_run_tracker }));
    extra.set_detailed_aggregated_metrics_events_holder();
    extra.set_blocking_executor(Arc::new(DummyBlockingExecutor { fs }));
    extra.set_materializer(Arc::new(NoDiskMaterializer));
    extra.set_re_client(UnconfiguredRemoteExecutionClient::testing_new_dummy());
    extra.set_http_client(HttpClientBuilder::https_with_system_roots().await?.build());
    extra.set_mergebase(Default::default());
    extra.data.set(EventDispatcher::null());
    extra.data.set(RunActionKnobs::default());
    extra.spawner = Arc::new(BuckSpawner::current_runtime().unwrap());

    let mut computations = dice_builder.build(extra).unwrap();
    inject_legacy_config_for_test(
        &mut computations,
        CellName::testing_new("root"),
        LegacyBuckConfig::empty(),
    )?;
    computations.set_buck_out_path(Some(output_path))?;
    computations.set_cell_resolver(cell_resolver)?;

    Ok(computations.commit().await)
}

#[tokio::test]
async fn test_get_action_for_artifact() -> bz_error::Result<()> {
    bz_certs::certs::maybe_setup_cryptography();
    let build_artifact = create_test_build_artifact();
    let registered_action = registered_action(
        build_artifact.dupe(),
        Box::new(SimpleAction::new(
            buck_indexset![],
            buck_indexset![build_artifact.dupe()],
            vec![],
            CategoryRef::new("fake_action").unwrap().to_owned(),
            None,
        )),
    );

    let mut dice_builder = DiceBuilder::new();
    dice_builder = mock_analysis_for_action_resolution(
        dice_builder,
        build_artifact.key(),
        registered_action.dupe(),
    );
    let mut dice_computations = dice_builder
        .build(UserComputationData::new())
        .unwrap()
        .commit()
        .await;

    let result = with_dispatcher_async(
        EventDispatcher::null(),
        ActionCalculation::get_action(&mut dice_computations, build_artifact.key()),
    )
    .await;
    assert_eq!(result?, registered_action);
    Ok(())
}

#[tokio::test]
async fn test_build_action() -> bz_error::Result<()> {
    bz_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;
    let build_artifact = create_test_build_artifact();
    let registered_action = registered_action(
        build_artifact.dupe(),
        Box::new(SimpleAction::new(
            buck_indexset![],
            buck_indexset![build_artifact.dupe()],
            vec!["foo".to_owned(), "cmd".to_owned()],
            CategoryRef::new("fake_action").unwrap().to_owned(),
            None,
        )),
    );

    let dry_run_tracker = Arc::new(Mutex::new(vec![]));
    let mut dice_computations = make_default_dice_state(
        dry_run_tracker.dupe(),
        &temp_fs,
        vec![{
            let action = registered_action.dupe();
            let action_key = build_artifact.key().dupe();
            Box::new(move |builder| {
                mock_analysis_for_action_resolution(builder, &action_key, action)
            })
        }],
    )
    .await?;

    let result =
        ActionCalculation::build_action(&mut dice_computations, registered_action.key()).await;

    result.unwrap();

    assert_eq!(
        dry_run_tracker.lock().unwrap()[0],
        DryRunEntry {
            args: vec!["foo".to_owned(), "cmd".to_owned()],
            outputs: vec![CommandExecutionOutput::BuildArtifact {
                path: build_artifact.get_path().dupe(),
                output_type: OutputType::File,
                produced_path: None,
            }],
            env: sorted_vector_map![]
        }
    );

    Ok(())
}

#[tokio::test]
async fn test_build_artifact() -> bz_error::Result<()> {
    bz_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;
    let build_artifact = create_test_build_artifact();
    let registered_action = registered_action(
        build_artifact.dupe(),
        Box::new(SimpleAction::new(
            buck_indexset![],
            buck_indexset![build_artifact.dupe()],
            vec!["bar".to_owned(), "cmd".to_owned()],
            CategoryRef::new("fake_action").unwrap().to_owned(),
            None,
        )),
    );

    let dry_run_tracker = Arc::new(Mutex::new(vec![]));
    let mut dice_computations = make_default_dice_state(dry_run_tracker.dupe(), &temp_fs, {
        let registered_action = registered_action.dupe();
        let action_key = build_artifact.key().dupe();
        vec![Box::new(move |builder| {
            mock_analysis_for_action_resolution(builder, &action_key, registered_action)
        })]
    })
    .await?;

    let result = with_dispatcher_async(
        EventDispatcher::null(),
        ActionCalculation::build_artifact(&mut dice_computations, &build_artifact),
    )
    .await;

    result.unwrap();

    assert_eq!(
        dry_run_tracker.lock().unwrap()[0],
        DryRunEntry {
            args: vec!["bar".to_owned(), "cmd".to_owned()],
            outputs: vec![CommandExecutionOutput::BuildArtifact {
                path: build_artifact.get_path().dupe(),
                output_type: OutputType::File,
                produced_path: None,
            }],
            env: sorted_vector_map![]
        }
    );
    Ok(())
}

#[tokio::test]
async fn test_ensure_artifact_build_artifact() -> bz_error::Result<()> {
    bz_certs::certs::maybe_setup_cryptography();
    let temp_fs = ProjectRootTemp::new()?;
    let build_artifact = create_test_build_artifact();
    let registered_action = registered_action(
        build_artifact.dupe(),
        Box::new(SimpleAction::new(
            buck_indexset![],
            buck_indexset![build_artifact.dupe()],
            vec!["ensure".to_owned(), "cmd".to_owned()],
            CategoryRef::new("fake_action").unwrap().to_owned(),
            None,
        )),
    );

    let dry_run_tracker = Arc::new(Mutex::new(vec![]));
    let mut dice_computations = make_default_dice_state(dry_run_tracker.dupe(), &temp_fs, {
        let registered_action = registered_action.dupe();
        let action_key = build_artifact.key().dupe();
        vec![Box::new(move |builder| {
            mock_analysis_for_action_resolution(builder, &action_key, registered_action)
        })]
    })
    .await?;

    let result = with_dispatcher_async(
        EventDispatcher::null(),
        dice_computations
            .ensure_artifact_group(&ArtifactGroup::Artifact(build_artifact.dupe().into())),
    )
    .await;

    result.unwrap();

    assert_eq!(
        dry_run_tracker.lock().unwrap()[0],
        DryRunEntry {
            args: vec!["ensure".to_owned(), "cmd".to_owned()],
            outputs: vec![CommandExecutionOutput::BuildArtifact {
                path: build_artifact.get_path().dupe(),
                output_type: OutputType::File,
                produced_path: None,
            }],
            env: sorted_vector_map![]
        }
    );

    Ok(())
}

#[tokio::test]
async fn test_ensure_artifact_source_artifact() -> bz_error::Result<()> {
    bz_certs::certs::maybe_setup_cryptography();
    let digest_config = DigestConfig::testing_default();

    let path = CellPath::new(
        CellName::testing_new("cell"),
        CellRelativePathBuf::unchecked_new("pkg/src.cpp".to_owned()),
    );
    let source_artifact = create_test_source_artifact("cell//pkg", "src.cpp");
    let metadata = FileMetadata {
        digest: TrackedFileDigest::from_content(b"content", digest_config.cas_digest_config()),
        is_executable: true,
    };

    let dice_builder = DiceBuilder::new().set_data(|data| {
        data.set_digest_config(DigestConfig::testing_default());
    });
    let file_ops = TestFileOps::new_with_files_metadata(btreemap![path => metadata.dupe()]);
    let mut dice_computations = file_ops
        .mock_in_cell(CellName::testing_new("cell"), dice_builder)
        .build(UserComputationData::new())
        .unwrap()
        .commit()
        .await;

    let source_artifact = Artifact::from(source_artifact);
    let input = ArtifactGroup::Artifact(source_artifact.dupe());
    let result = with_dispatcher_async(
        EventDispatcher::null(),
        dice_computations.ensure_artifact_group(&input),
    )
    .await?
    .iter()
    .cloned()
    .collect::<Vec<_>>();

    assert_eq!(
        &result,
        &[(
            source_artifact,
            ArtifactValue::file(FileMetadata {
                digest: metadata.digest,
                is_executable: metadata.is_executable,
            })
        )],
    );
    Ok(())
}

#[tokio::test]
async fn test_ensure_artifact_external_symlink() -> bz_error::Result<()> {
    bz_certs::certs::maybe_setup_cryptography();
    let path = CellPath::new(
        CellName::testing_new("cell"),
        CellRelativePathBuf::unchecked_new("proj/to_gvfs/include".to_owned()),
    );
    let source_artifact = create_test_source_artifact("cell//proj/to_gvfs", "include");
    let symlink = Arc::new(
        ExternalSymlink::new(
            PathBuf::from("/mnt/gvfs"),
            ForwardRelativePathBuf::new("include".to_owned()).unwrap(),
        )
        .unwrap(),
    );

    let dice_builder = DiceBuilder::new().set_data(|data| {
        data.set_digest_config(DigestConfig::testing_default());
    });
    let file_ops = TestFileOps::new_with_symlinks(btreemap![path => symlink.dupe()]);
    let mut dice_computations = file_ops
        .mock_in_cell(CellName::testing_new("cell"), dice_builder)
        .build(UserComputationData::new())
        .unwrap()
        .commit()
        .await;

    let source_artifact = Artifact::from(source_artifact);
    let input = ArtifactGroup::Artifact(source_artifact.dupe());
    let result = with_dispatcher_async(
        EventDispatcher::null(),
        dice_computations.ensure_artifact_group(&input),
    )
    .await?
    .iter()
    .cloned()
    .collect::<Vec<_>>();

    assert_eq!(
        &result,
        &[(
            source_artifact,
            ArtifactValue::new(
                DirectoryEntry::Leaf(ActionDirectoryMember::ExternalSymlink(symlink)),
                None
            )
        )]
    );
    Ok(())
}

#[tokio::test]
async fn test_command_details_omission() {
    use bz_data::command_execution_kind::Command;

    bz_certs::certs::maybe_setup_cryptography();
    let digest_config = DigestConfig::testing_default();

    let mut report = CommandExecutionReport {
        claim: None,
        status: CommandExecutionStatus::Success {
            execution_kind: CommandExecutionKind::Local {
                digest: ActionDigest::empty(digest_config.cas_digest_config()),
                command: vec![],
                env: sorted_vector_map![],
            },
        },
        timing: CommandExecutionMetadata::empty(TimeSpan::empty_now()),
        std_streams: CommandStdStreams::Local {
            stdout: "stdout".to_owned().into_bytes(),
            stderr: "stderr".to_owned().into_bytes(),
        },
        exit_code: Some(1),
        additional_message: None,
        inline_environment_metadata: bz_data::InlineCommandExecutionEnvironmentMetadata {
            sandcastle_instance_id: Some(123),
        },
    };

    let proto = command_details(&report, false).await;
    let command_kind = proto.command_kind.unwrap();
    assert_matches!(command_kind.command, Some(Command::LocalCommand(..)));
    assert_eq!(&proto.cmd_stdout, "stdout");
    assert_eq!(&proto.cmd_stderr, "stderr");

    let proto = command_details(&report, true).await;
    let command_kind = proto.command_kind.unwrap();
    assert_matches!(command_kind.command, Some(Command::OmittedLocalCommand(..)));
    assert_eq!(&proto.cmd_stdout, "");
    assert_eq!(&proto.cmd_stderr, "stderr");

    report.status = CommandExecutionStatus::Failure {
        execution_kind: CommandExecutionKind::Local {
            digest: ActionDigest::empty(digest_config.cas_digest_config()),
            command: vec![],
            env: sorted_vector_map![],
        },
    };
    let proto = command_details(&report, true).await;
    let command_kind = proto.command_kind.unwrap();
    assert_matches!(command_kind.command, Some(Command::LocalCommand(..)));
    assert_eq!(&proto.cmd_stdout, "stdout");
    assert_eq!(&proto.cmd_stderr, "stderr");
}

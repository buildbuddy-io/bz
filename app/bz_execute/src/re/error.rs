/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use allocative::Allocative;
use bz_error::ErrorTag;
use bz_error::TypedContext;
use remote_execution::TCode;
use remote_execution::TCodeReasonGroup;
use remote_execution::re_client_error_from_anyhow;

pub fn get_re_error_tag(tcode: &TCode) -> ErrorTag {
    match *tcode {
        TCode::CANCELLED => ErrorTag::ReCancelled,
        TCode::UNKNOWN => ErrorTag::ReUnknown,
        TCode::INVALID_ARGUMENT => ErrorTag::ReInvalidArgument,
        TCode::DEADLINE_EXCEEDED => ErrorTag::ReDeadlineExceeded,
        TCode::NOT_FOUND => ErrorTag::ReNotFound,
        TCode::ALREADY_EXISTS => ErrorTag::ReAlreadyExists,
        TCode::PERMISSION_DENIED => ErrorTag::RePermissionDenied,
        TCode::RESOURCE_EXHAUSTED => ErrorTag::ReResourceExhausted,
        TCode::FAILED_PRECONDITION => ErrorTag::ReFailedPrecondition,
        TCode::ABORTED => ErrorTag::ReAborted,
        TCode::OUT_OF_RANGE => ErrorTag::ReOutOfRange,
        TCode::UNIMPLEMENTED => ErrorTag::ReUnimplemented,
        TCode::INTERNAL => ErrorTag::ReInternal,
        TCode::UNAVAILABLE => ErrorTag::ReUnavailable,
        TCode::DATA_LOSS => ErrorTag::ReDataLoss,
        TCode::UNAUTHENTICATED => ErrorTag::ReUnauthenticated,
        _ => ErrorTag::ReUnknownTcode,
    }
}

pub fn get_re_group_tag(group: &TCodeReasonGroup) -> Option<ErrorTag> {
    match *group {
        TCodeReasonGroup::RE_CONNECTION => Some(ErrorTag::ReConnection),
        TCodeReasonGroup::USER_QUOTA => Some(ErrorTag::ReUserQuota),
        TCodeReasonGroup::USER_BAD_CERTS => Some(ErrorTag::ReUserBadCerts),
        _ => None,
    }
}

#[derive(Allocative, Debug, Clone, bz_error::Error)]
#[error("Remote Execution Error on {} for ReSession {}\nError: ({})", .re_action, .re_session_id, .message)]
#[buck2(tag = get_re_error_tag(code))]
pub struct RemoteExecutionError {
    re_action: String,
    re_session_id: String,
    pub message: String,
    #[allocative(skip)]
    pub code: TCode,
    #[allocative(skip)]
    pub group: TCodeReasonGroup,
}

impl TypedContext for RemoteExecutionError {
    fn eq(&self, other: &dyn TypedContext) -> bool {
        match (other as &dyn std::any::Any).downcast_ref::<Self>() {
            Some(right) => self.eq(right),
            None => false,
        }
    }

    fn display(&self) -> Option<String> {
        None
    }
}

pub fn is_re_auth_or_permission_error(error: &RemoteExecutionError) -> bool {
    if is_re_worker_setup_permission_error(error) {
        return false;
    }

    if matches!(
        error.code,
        TCode::PERMISSION_DENIED | TCode::UNAUTHENTICATED
    ) {
        return true;
    }

    let message = error.message.to_ascii_lowercase();
    message.contains("permissiondenied desc")
        || message.contains("permission denied")
        || message.contains("unauthenticated")
        || message.contains("missing api key")
        || message.contains("invalid api key")
}

pub fn is_re_worker_setup_permission_error(error: &RemoteExecutionError) -> bool {
    let message = error.message.to_ascii_lowercase();
    message.contains("permission denied")
        && message.contains("create oci bundle")
        && message.contains("cgroup")
}

pub(crate) fn re_error(
    re_action: &str,
    re_session_id: &str,
    message: String,
    code: TCode,
    group: TCodeReasonGroup,
) -> bz_error::Error {
    let re_error = RemoteExecutionError {
        re_action: re_action.to_owned(),
        re_session_id: re_session_id.to_owned(),
        message,
        code,
        group,
    };
    let error = bz_error::Error::from(re_error.clone()).context(re_error);
    if let Some(tag) = get_re_group_tag(&group) {
        error.tag([tag])
    } else {
        error.string_tag(&group.to_string())
    }
}

pub(crate) async fn with_error_handler<T>(
    re_action: &str,
    re_session_id: &str,
    result: anyhow::Result<T>,
) -> bz_error::Result<T> {
    match result {
        Ok(val) => Ok(val),
        Err(e) => {
            let (code, group) = re_client_error_from_anyhow(&e)
                .map_or((TCode::UNKNOWN, TCodeReasonGroup::UNKNOWN), |e| {
                    (e.code, e.group)
                });

            Err(re_error(
                re_action,
                re_session_id,
                format!("{e:#}"),
                code,
                group,
            ))
        }
    }
}

pub fn test_re_error(message: &str, code: TCode) -> bz_error::Error {
    re_error(
        "test",
        "test",
        message.to_owned(),
        code,
        TCodeReasonGroup::UNKNOWN,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_re_error() {
        let error: bz_error::Error = re_error(
            "test",
            "test",
            "test".to_owned(),
            TCode::UNKNOWN,
            TCodeReasonGroup::UNKNOWN,
        );

        let err = error.find_typed_context::<RemoteExecutionError>().unwrap();
        assert_eq!(err.code, TCode::UNKNOWN);
    }
}

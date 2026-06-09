/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::fmt;
use std::fmt::Display;

use dupe::Dupe;
use thiserror::Error;
use tonic::Code;

#[derive(Error, Debug, Clone, Default)]
#[error("{}", .message)]
pub struct REClientError {
    pub message: String,
    pub code: TCode,
    pub group: TCodeReasonGroup,
}

pub fn re_client_error_from_anyhow(error: &anyhow::Error) -> Option<REClientError> {
    for error in error.chain() {
        if let Some(error) = error.downcast_ref::<REClientError>() {
            return Some(error.clone());
        }

        if let Some(status) = error.downcast_ref::<tonic::Status>() {
            return Some(REClientError {
                message: status.message().to_owned(),
                code: TCode::from_tonic(status.code()),
                group: TCodeReasonGroup::UNKNOWN,
            });
        }
    }

    None
}

#[derive(Debug, Clone, Dupe, Default)]
pub struct ErrorLocation(pub ::std::primitive::i32);

impl Display for ErrorLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UNKNOWN")
    }
}

#[derive(Copy, Debug, PartialEq, Eq, Clone, Dupe, Default)]
pub struct TCode(pub i32);

impl TCode {
    pub const OK: Self = TCode(0i32);
    pub const CANCELLED: Self = TCode(1i32);
    pub const UNKNOWN: Self = TCode(2i32);
    pub const INVALID_ARGUMENT: Self = TCode(3i32);
    pub const DEADLINE_EXCEEDED: Self = TCode(4i32);
    pub const NOT_FOUND: Self = TCode(5i32);
    pub const ALREADY_EXISTS: Self = TCode(6i32);
    pub const PERMISSION_DENIED: Self = TCode(7i32);
    pub const RESOURCE_EXHAUSTED: Self = TCode(8i32);
    pub const FAILED_PRECONDITION: Self = TCode(9i32);
    pub const ABORTED: Self = TCode(10i32);
    pub const OUT_OF_RANGE: Self = TCode(11i32);
    pub const UNIMPLEMENTED: Self = TCode(12i32);
    pub const INTERNAL: Self = TCode(13i32);
    pub const UNAVAILABLE: Self = TCode(14i32);
    pub const DATA_LOSS: Self = TCode(15i32);
    pub const UNAUTHENTICATED: Self = TCode(16i32);

    fn from_tonic(code: Code) -> Self {
        match code {
            Code::Ok => TCode::OK,
            Code::Cancelled => TCode::CANCELLED,
            Code::Unknown => TCode::UNKNOWN,
            Code::InvalidArgument => TCode::INVALID_ARGUMENT,
            Code::DeadlineExceeded => TCode::DEADLINE_EXCEEDED,
            Code::NotFound => TCode::NOT_FOUND,
            Code::AlreadyExists => TCode::ALREADY_EXISTS,
            Code::PermissionDenied => TCode::PERMISSION_DENIED,
            Code::ResourceExhausted => TCode::RESOURCE_EXHAUSTED,
            Code::FailedPrecondition => TCode::FAILED_PRECONDITION,
            Code::Aborted => TCode::ABORTED,
            Code::OutOfRange => TCode::OUT_OF_RANGE,
            Code::Unimplemented => TCode::UNIMPLEMENTED,
            Code::Internal => TCode::INTERNAL,
            Code::Unavailable => TCode::UNAVAILABLE,
            Code::DataLoss => TCode::DATA_LOSS,
            Code::Unauthenticated => TCode::UNAUTHENTICATED,
        }
    }
}

impl Display for TCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self == &TCode::OK {
            write!(f, "OK")
        } else if self == &TCode::INVALID_ARGUMENT {
            write!(f, "INVALID_ARGUMENT")
        } else {
            write!(f, "UNKNOWN")
        }
    }
}

#[derive(Copy, Debug, PartialEq, Eq, Clone, Dupe, Default)]
pub struct TCodeReasonGroup(pub i32);

impl TCodeReasonGroup {
    pub const UNKNOWN: Self = TCodeReasonGroup(0i32);
    pub const RE_CONNECTION: Self = TCodeReasonGroup(1i32);
    pub const USER_QUOTA: Self = TCodeReasonGroup(2i32);
    pub const USER_BAD_CERTS: Self = TCodeReasonGroup(3i32);
    pub const USER_QUEUE_FULL: Self = TCodeReasonGroup(2023i32);
}

impl Display for TCodeReasonGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TCodeReasonGroup::RE_CONNECTION => write!(f, "RE_CONNECTION"),
            TCodeReasonGroup::USER_QUOTA => write!(f, "USER_QUOTA"),
            TCodeReasonGroup::USER_BAD_CERTS => write!(f, "USER_BAD_CERTS"),
            TCodeReasonGroup::USER_QUEUE_FULL => write!(f, "USER_QUEUE_FULL"),
            _ => write!(f, "UNKNOWN"),
        }
    }
}

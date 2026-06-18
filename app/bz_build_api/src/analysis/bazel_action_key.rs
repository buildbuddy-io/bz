use bz_core::cells::external::bzlmod_canonical_repo_name_for_cell;
use bz_core::configuration::constraints::ConstraintKey;
use bz_core::configuration::constraints::ConstraintValue;
use bz_core::execution_types::execution::ExecutionPlatform;
use bz_core::execution_types::execution::ExecutionPlatformResolution;
use bz_core::provider::label::ProvidersLabel;
use bz_core::target::label::label::TargetLabel;
use sha2::Digest;
use sha2::Sha256;

/// Fingerprinter compatible with Bazel's
/// `com.google.devtools.build.lib.util.Fingerprint`.
pub struct BazelFingerprint {
    hasher: Sha256,
}

impl BazelFingerprint {
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    pub fn add_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.hasher.update(bytes);
        self
    }

    pub fn add_bool(&mut self, value: bool) -> &mut Self {
        self.hasher.update([u8::from(value)]);
        self
    }

    pub fn add_int(&mut self, value: i32) -> &mut Self {
        if value >= 0 {
            self.add_varint(value as u64);
        } else {
            self.add_varint(value as i64 as u64);
        }
        self
    }

    pub fn add_long_bits(&mut self, value: u64) -> &mut Self {
        self.add_varint(value);
        self
    }

    pub fn add_uuid(
        &mut self,
        most_significant_bits: u64,
        least_significant_bits: u64,
    ) -> &mut Self {
        self.add_long_bits(least_significant_bits);
        self.add_long_bits(most_significant_bits);
        self
    }

    pub fn add_string(&mut self, value: &str) -> &mut Self {
        self.add_varint(value.len() as u64);
        self.hasher.update(value.as_bytes());
        self
    }

    pub fn add_path(&mut self, value: &str) -> &mut Self {
        self.add_string(value)
    }

    pub fn add_string_map<'a>(
        &mut self,
        entries: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> &mut Self {
        let entries = entries.into_iter().collect::<Vec<_>>();
        self.add_int(entries.len() as i32);
        for (key, value) in entries {
            self.add_string(key);
            self.add_string(value);
        }
        self
    }

    pub fn add_strings<'a>(&mut self, entries: impl IntoIterator<Item = &'a str>) -> &mut Self {
        let entries = entries.into_iter().collect::<Vec<_>>();
        self.add_int(entries.len() as i32);
        for entry in entries {
            self.add_string(entry);
        }
        self
    }

    pub fn add_nullable_string(&mut self, value: Option<&str>) -> &mut Self {
        if let Some(value) = value {
            self.add_bool(true);
            self.add_string(value);
        } else {
            self.add_bool(false);
        }
        self
    }

    pub fn finalize_hex(self) -> String {
        hex::encode(self.hasher.finalize())
    }

    fn add_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.hasher.update([((value as u8) & 0x7f) | 0x80]);
            value >>= 7;
        }
        self.hasher.update([value as u8]);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BazelActionKey(String);

impl BazelActionKey {
    /// Finalizes a fingerprint for Bazel actions that override their execution platform to
    /// `PlatformInfo.EMPTY_PLATFORM_INFO` and have no exec properties.
    fn from_platform_agnostic_fingerprint(mut fingerprint: BazelFingerprint) -> Self {
        add_bazel_empty_execution_platform_to_action_key(&mut fingerprint);
        fingerprint.add_string_map(std::iter::empty());
        fingerprint.add_int(BAZEL_ACTION_KEY_UNIQUIFIER);
        Self(fingerprint.finalize_hex())
    }

    fn from_owner_fingerprint(
        mut fingerprint: BazelFingerprint,
        owner_key: &BazelActionOwnerKey,
    ) -> Self {
        if let Some(execution_platform) = &owner_key.execution_platform {
            fingerprint.add_bool(true);
            add_bazel_platform_info_to_action_key(&mut fingerprint, execution_platform);
        } else {
            fingerprint.add_bool(false);
        }
        fingerprint.add_string_map(
            owner_key
                .exec_properties
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        fingerprint.add_int(BAZEL_ACTION_KEY_UNIQUIFIER);
        Self(fingerprint.finalize_hex())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BazelActionOwnerKey {
    execution_platform: Option<BazelPlatformInfoKey>,
    exec_properties: Vec<(String, String)>,
}

impl BazelActionOwnerKey {
    pub fn new(
        execution_platform: &ExecutionPlatformResolution,
        exec_properties: impl IntoIterator<Item = (String, String)>,
    ) -> bz_error::Result<Option<Self>> {
        let execution_platform = match execution_platform.platform() {
            Ok(execution_platform) => {
                let Some(platform_key) =
                    BazelPlatformInfoKey::from_execution_platform(execution_platform)?
                else {
                    return Ok(None);
                };
                Some(platform_key)
            }
            Err(_) => return Ok(None),
        };
        let exec_properties = exec_properties.into_iter().collect::<Vec<_>>();
        Ok(Some(Self {
            execution_platform,
            exec_properties,
        }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BazelPlatformInfoKey {
    label: String,
    constraints: Vec<(String, String)>,
    exec_properties: Vec<(String, String)>,
}

impl BazelPlatformInfoKey {
    fn from_execution_platform(
        execution_platform: &ExecutionPlatform,
    ) -> bz_error::Result<Option<Self>> {
        let Some(target) = execution_platform.target_label() else {
            return Ok(None);
        };
        let constraints = execution_platform
            .cfg()
            .data()?
            .constraints
            .iter()
            .map(|(key, value)| {
                (
                    bazel_canonical_label_for_constraint_setting(key),
                    bazel_canonical_label_for_constraint_value(value),
                )
            })
            .collect::<Vec<_>>();
        Ok(Some(Self {
            label: bazel_canonical_label_for_target(target),
            constraints,
            exec_properties: Vec::new(),
        }))
    }
}

const BAZEL_ACTION_KEY_UNIQUIFIER: i32 = 0;
const BAZEL_INTERNAL_PLATFORM_LABEL: &str = "@bazel_tools//tools:internal_platform";
const BAZEL_SYMLINK_ACTION_GUID: &str = "7f4fab4d-d0a7-4f0f-8649-1d0337a21fee";
const BAZEL_UNRESOLVED_SYMLINK_ACTION_GUID: &str = "0f302651-602c-404b-881c-58913193cfe7";

fn bazel_canonical_repo_for_cell(cell: &str) -> Option<String> {
    if cell == "root" {
        Some(String::new())
    } else if cell == "bazel_tools" {
        Some("bazel_tools".to_owned())
    } else {
        bzlmod_canonical_repo_name_for_cell(cell)
            .or_else(|| cell.strip_prefix("bzlmod_").map(str::to_owned))
    }
}

fn bazel_canonical_label_for_target(label: &TargetLabel) -> String {
    let package = label.pkg();
    let repo = bazel_canonical_repo_for_cell(package.cell_name().as_str())
        .unwrap_or_else(|| package.cell_name().to_string());
    format!(
        "@@{}//{}:{}",
        repo,
        package.cell_relative_path().as_str(),
        label.name()
    )
}

fn bazel_canonical_label_for_constraint_setting(key: &ConstraintKey) -> String {
    bazel_canonical_label_for_target(&key.key)
}

fn bazel_canonical_label_for_constraint_value(value: &ConstraintValue) -> String {
    let providers_label: &ProvidersLabel = &value.0;
    bazel_canonical_label_for_target(providers_label.target())
}

fn add_bazel_empty_execution_platform_to_action_key(fingerprint: &mut BazelFingerprint) {
    fingerprint.add_bool(true);
    fingerprint.add_string(BAZEL_INTERNAL_PLATFORM_LABEL);
    fingerprint.add_bool(false);
    fingerprint.add_int(0);
    fingerprint.add_string_map(std::iter::empty());
    fingerprint.add_strings(std::iter::empty());
    fingerprint.add_strings(std::iter::empty());
    fingerprint.add_strings(std::iter::empty());
    fingerprint.add_bool(false);
    fingerprint.add_nullable_string(None);
}

fn add_bazel_platform_info_to_action_key(
    fingerprint: &mut BazelFingerprint,
    platform: &BazelPlatformInfoKey,
) {
    fingerprint.add_string(&platform.label);
    fingerprint.add_bool(false);
    fingerprint.add_int(platform.constraints.len() as i32);
    for (constraint_setting, constraint_value) in &platform.constraints {
        fingerprint.add_string(constraint_setting);
        fingerprint.add_string(constraint_value);
    }
    fingerprint.add_string_map(
        platform
            .exec_properties
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    fingerprint.add_strings(std::iter::empty());
    fingerprint.add_strings(std::iter::empty());
    fingerprint.add_strings(std::iter::empty());
    fingerprint.add_bool(false);
    fingerprint.add_nullable_string(None);
}

pub fn bazel_symlink_action_key(input_path: Option<&str>) -> BazelActionKey {
    let mut fingerprint = BazelFingerprint::new();
    fingerprint.add_string(BAZEL_SYMLINK_ACTION_GUID);
    if let Some(input_path) = input_path {
        fingerprint.add_path(input_path);
    }
    BazelActionKey::from_platform_agnostic_fingerprint(fingerprint)
}

pub fn bazel_unresolved_symlink_action_key(
    target: &str,
    target_type: Option<&str>,
    owner_key: &BazelActionOwnerKey,
) -> BazelActionKey {
    let mut fingerprint = BazelFingerprint::new();
    fingerprint.add_string(BAZEL_UNRESOLVED_SYMLINK_ACTION_GUID);
    fingerprint.add_string(target);
    fingerprint.add_string(match target_type {
        Some("file") => "FILE",
        Some("directory") => "DIRECTORY",
        _ => "UNSPECIFIED",
    });
    BazelActionKey::from_owner_fingerprint(fingerprint, owner_key)
}

pub fn bazel_solib_symlink_action_key(
    symlink_exec_path: &str,
    input_exec_path: &str,
) -> BazelActionKey {
    let mut fingerprint = BazelFingerprint::new();
    fingerprint
        .add_path(symlink_exec_path)
        .add_path(input_exec_path);
    BazelActionKey::from_platform_agnostic_fingerprint(fingerprint)
}

pub fn bazel_spawn_action_key(
    fingerprint: BazelFingerprint,
    owner_key: &BazelActionOwnerKey,
) -> BazelActionKey {
    BazelActionKey::from_owner_fingerprint(fingerprint, owner_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_bazel_empty_digest() {
        assert_eq!(
            BazelFingerprint::new().finalize_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn fingerprint_uses_protobuf_string_framing() {
        let mut fingerprint = BazelFingerprint::new();
        fingerprint.add_string("abc");
        assert_eq!(
            fingerprint.finalize_hex(),
            hex::encode(Sha256::digest([3, b'a', b'b', b'c']))
        );
    }
}

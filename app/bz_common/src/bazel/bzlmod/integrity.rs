use base64::Engine;
use buck2_error::buck2_error;
use sha1::Sha1;
use sha2::Digest;
use sha2::Sha256;
use sha2::Sha384;
use sha2::Sha512;

pub struct BzlmodIntegrity {
    kind: BzlmodIntegrityKind,
    bytes: Vec<u8>,
}

impl BzlmodIntegrity {
    pub fn kind(&self) -> BzlmodIntegrityKind {
        self.kind
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Copy)]
pub enum BzlmodIntegrityKind {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl BzlmodIntegrityKind {
    fn all() -> &'static [Self] {
        &[Self::Sha1, Self::Sha256, Self::Sha384, Self::Sha512]
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1-",
            Self::Sha256 => "sha256-",
            Self::Sha384 => "sha384-",
            Self::Sha512 => "sha512-",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
        }
    }

    fn byte_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
            Self::Sha384 => 48,
            Self::Sha512 => 64,
        }
    }

    pub fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => Sha1::digest(bytes).to_vec(),
            Self::Sha256 => Sha256::digest(bytes).to_vec(),
            Self::Sha384 => Sha384::digest(bytes).to_vec(),
            Self::Sha512 => Sha512::digest(bytes).to_vec(),
        }
    }
}

pub fn parse_bzlmod_integrity(integrity: &str) -> buck2_error::Result<Option<BzlmodIntegrity>> {
    if integrity.is_empty() {
        return Ok(None);
    }

    for &kind in BzlmodIntegrityKind::all() {
        let Some(encoded) = integrity.strip_prefix(kind.prefix()) else {
            continue;
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| {
                buck2_error!(
                    buck2_error::ErrorTag::Input,
                    "invalid base64 in bzlmod integrity `{}`",
                    integrity
                )
            })?;
        if bytes.len() != kind.byte_len() {
            return Err(buck2_error!(
                buck2_error::ErrorTag::Input,
                "invalid bzlmod {} integrity `{}`",
                kind.name(),
                integrity
            ));
        }
        return Ok(Some(BzlmodIntegrity { kind, bytes }));
    }

    Err(buck2_error!(
        buck2_error::ErrorTag::Input,
        "unsupported bzlmod integrity `{}`",
        integrity
    ))
}

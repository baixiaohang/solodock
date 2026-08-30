use super::{Platform, RegistryError, reference::validate_digest};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageIdentity {
    manifest_digest: String,
    config_digest: String,
    platform: Platform,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManifestDescriptor {
    pub digest: Option<String>,
    pub os: Option<String>,
    pub architecture: Option<String>,
    pub variant: Option<String>,
}

impl ImageIdentity {
    pub fn new(
        manifest_digest: &str,
        config_digest: &str,
        platform: &Platform,
    ) -> Result<Self, RegistryError> {
        validate_digest(manifest_digest)?;
        validate_digest(config_digest)?;
        let platform = Platform::canonical(
            &platform.os,
            &platform.architecture,
            platform.variant.as_deref(),
        )?;
        Ok(Self {
            manifest_digest: manifest_digest.to_owned(),
            config_digest: config_digest.to_owned(),
            platform,
        })
    }

    pub fn matches_engine_image_id(&self, observed: Option<&str>) -> bool {
        observed.is_some_and(|value| {
            validate_digest(value).is_ok()
                && (value == self.config_digest || value == self.manifest_digest)
        })
    }

    pub fn matches_manifest_descriptor(&self, descriptor: &ManifestDescriptor) -> bool {
        let Some(digest) = descriptor.digest.as_deref() else {
            return false;
        };
        let (Some(os), Some(architecture)) =
            (descriptor.os.as_deref(), descriptor.architecture.as_deref())
        else {
            return false;
        };
        validate_digest(digest).is_ok()
            && digest == self.manifest_digest
            && Platform::canonical(os, architecture, descriptor.variant.as_deref())
                .is_ok_and(|platform| platform == self.platform)
    }

    pub fn matches_observation(
        &self,
        engine_image_id: Option<&str>,
        descriptor: Option<&ManifestDescriptor>,
    ) -> bool {
        self.matches_engine_image_id(engine_image_id)
            && descriptor.is_none_or(|value| self.matches_manifest_descriptor(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ImageIdentity {
        ImageIdentity::new(
            &format!("sha256:{}", "a".repeat(64)),
            &format!("sha256:{}", "b".repeat(64)),
            &Platform::canonical("linux", "amd64", None).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn engine_id_accepts_only_valid_config_or_manifest_digest() {
        let identity = identity();
        assert!(identity.matches_engine_image_id(Some(&format!("sha256:{}", "a".repeat(64)))));
        assert!(identity.matches_engine_image_id(Some(&format!("sha256:{}", "b".repeat(64)))));
        assert!(!identity.matches_engine_image_id(Some(&format!("sha256:{}", "c".repeat(64)))));
        assert!(!identity.matches_engine_image_id(Some("sha256:short")));
        assert!(!identity.matches_engine_image_id(None));
    }

    #[test]
    fn present_descriptor_must_match_digest_and_canonical_platform() {
        let identity = identity();
        let valid = ManifestDescriptor {
            digest: Some(format!("sha256:{}", "a".repeat(64))),
            os: Some("LINUX".into()),
            architecture: Some("x86_64".into()),
            variant: None,
        };
        assert!(identity.matches_manifest_descriptor(&valid));
        assert!(
            identity.matches_observation(Some(&format!("sha256:{}", "b".repeat(64))), Some(&valid))
        );
        assert!(identity.matches_observation(Some(&format!("sha256:{}", "b".repeat(64))), None));

        let mut wrong_digest = valid.clone();
        wrong_digest.digest = Some(format!("sha256:{}", "c".repeat(64)));
        assert!(!identity.matches_manifest_descriptor(&wrong_digest));
        let mut missing_platform = valid.clone();
        missing_platform.architecture = None;
        assert!(!identity.matches_manifest_descriptor(&missing_platform));
        let mut wrong_platform = valid;
        wrong_platform.architecture = Some("arm64".into());
        assert!(!identity.matches_manifest_descriptor(&wrong_platform));
    }
}

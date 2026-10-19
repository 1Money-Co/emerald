use shadow_rs::shadow;

shadow!(build_info);

/// Immutable metadata compiled into the running Emerald binary.
#[derive(Clone, Copy, Debug)]
pub struct BuildMetadata {
    pub service_version: &'static str,
    pub service_build_time: &'static str,
    pub service_rust_version: &'static str,
    pub vcs_ref_head: &'static str,
    pub vcs_ref_head_branch: &'static str,
}

impl BuildMetadata {
    pub fn current() -> Self {
        Self::new(
            build_info::PKG_VERSION,
            build_info::BUILD_TIME_3339,
            build_info::RUST_VERSION,
            build_info::COMMIT_HASH,
            build_info::BRANCH,
        )
    }

    fn new(
        service_version: &'static str,
        service_build_time: &'static str,
        service_rust_version: &'static str,
        vcs_ref_head: &'static str,
        vcs_ref_head_branch: &'static str,
    ) -> Self {
        let vcs_ref_head = available(vcs_ref_head);
        let vcs_ref_head_branch = match vcs_ref_head_branch.trim() {
            "HEAD" => "detached",
            "" if vcs_ref_head != "unknown" => "detached",
            _ => available(vcs_ref_head_branch),
        };
        Self {
            service_version: available(service_version),
            service_build_time: available(service_build_time),
            service_rust_version: available(service_rust_version),
            vcs_ref_head,
            vcs_ref_head_branch,
        }
    }

    pub fn labels(self) -> [(&'static str, &'static str); 5] {
        [
            ("service_version", self.service_version),
            ("service_build_time", self.service_build_time),
            ("service_rust_version", self.service_rust_version),
            ("vcs_ref_head", self.vcs_ref_head),
            ("vcs_ref_head_branch", self.vcs_ref_head_branch),
        ]
    }
}

fn available(value: &'static str) -> &'static str {
    if value.trim().is_empty() {
        "unknown"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_metadata_has_explicit_fallbacks() {
        let metadata = BuildMetadata::new("", " ", "", "", "");
        assert_eq!(
            metadata.labels(),
            [
                ("service_version", "unknown"),
                ("service_build_time", "unknown"),
                ("service_rust_version", "unknown"),
                ("vcs_ref_head", "unknown"),
                ("vcs_ref_head_branch", "unknown"),
            ]
        );
    }

    #[test]
    fn detached_head_retains_commit_and_marks_branch() {
        let metadata = BuildMetadata::new(
            "0.0.1",
            "2026-10-19T10:00:00+00:00",
            "rustc 1.91.1",
            "ef0c236f62bb8a44c3063c08297636e6fea05d83",
            "HEAD",
        );
        assert_eq!(metadata.vcs_ref_head_branch, "detached");
        assert_eq!(
            metadata.vcs_ref_head,
            "ef0c236f62bb8a44c3063c08297636e6fea05d83"
        );
    }

    #[test]
    fn missing_branch_with_known_commit_is_detached() {
        let metadata = BuildMetadata::new(
            "0.0.1",
            "",
            "",
            "ef0c236f62bb8a44c3063c08297636e6fea05d83",
            "",
        );
        assert_eq!(metadata.vcs_ref_head_branch, "detached");
    }
}

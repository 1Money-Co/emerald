use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use shadow_rs::{
    default_deny, BuildPattern, ShadowBuilder, ShadowError, BRANCH, COMMIT_HASH, SHORT_COMMIT, TAG,
};

fn main() -> Result<(), ShadowError> {
    // Cargo can reuse the build-script executable for another copy of this crate.
    // Resolve the current source directory when the script runs, not when it is compiled.
    let root = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    )
    .join("../..");
    // Git discovery walks into parent repositories. An archive must never inherit
    // the identity of the checkout into which it happens to have been extracted.
    // Both ordinary checkouts (.git directory) and worktrees (.git file) are valid.
    let has_repository = root.join(".git").exists();
    let mut deny = default_deny();
    if !has_repository {
        deny.extend([BRANCH, COMMIT_HASH, SHORT_COMMIT, TAG]);
    }

    ShadowBuilder::builder()
        // Refresh metadata when HEAD/ref changes, including cached debug builds.
        .build_pattern(BuildPattern::RealTime)
        .deny_const(deny)
        .hook(move |mut file: &File| -> shadow_rs::SdResult<()> {
            if !has_repository {
                for name in [BRANCH, COMMIT_HASH, SHORT_COMMIT] {
                    writeln!(file, "pub const {name}: &str = \"unknown\";")?;
                }
                writeln!(file, "pub const TAG: &str = \"\";")?;
            }
            Ok(())
        })
        .build()
        .map(|_| ())
}

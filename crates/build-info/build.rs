use shadow_rs::{BuildPattern, ShadowBuilder, ShadowError};

fn main() -> Result<(), ShadowError> {
    ShadowBuilder::builder()
        .build_pattern(BuildPattern::Lazy)
        .build()
        .map(|_| ())
}

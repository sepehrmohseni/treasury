//! Containers for integration tests.

use std::path::PathBuf;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncBuilder;
use testcontainers::{GenericBuildableImage, GenericImage, ImageExt};

/// The `tests/` directory.
pub fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The OpenSSH test server image, built from `tests/docker/sshd` once and reused.
pub async fn sshd_image() -> GenericImage {
    let dir = tests_dir().join("docker").join("sshd");
    GenericBuildableImage::new("treasury-test-sshd", "1")
        .with_dockerfile(dir.join("Dockerfile"))
        .with_file(dir.join("sshd_config"), "sshd_config")
        .with_file(dir.join("entrypoint.sh"), "entrypoint.sh")
        .build_image_with(testcontainers::core::BuildImageOptions::new().with_skip_if_exists(true))
        .await
        .expect("build the sshd test image")
        .with_exposed_port(22.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server listening on"))
}

/// How long a container may take to start before the test fails.
pub const STARTUP: Duration = Duration::from_secs(120);

/// Applies the common start timeout.
pub fn with_startup(image: GenericImage) -> testcontainers::ContainerRequest<GenericImage> {
    image.with_startup_timeout(STARTUP)
}

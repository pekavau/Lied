//! Helpers shared by the integration tests that need object storage. Each
//! file under `tests/` is its own crate and declares `mod common;`, so a
//! helper one of them does not use is not dead code in general.
#![allow(dead_code)]

use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// The object store Lied ships with (#59): RustFS, pinned. The same tag as
/// `docker-compose.yml`; bump both together.
pub const OBJECT_STORE_IMAGE: &str = "rustfs/rustfs";
pub const OBJECT_STORE_TAG: &str = "1.0.5";

/// The test store's credentials.
pub const ACCESS_KEY: &str = "lied-test";
pub const SECRET_KEY: &str = "lied-test-secret";

/// Start a RustFS container and wait until it serves requests. Returns the
/// container (keep it alive for the test) and its S3 endpoint URL.
///
/// RustFS logs nothing when it becomes ready, so this waits for its start
/// line and then polls `/health/ready`.
pub async fn start_object_store() -> (ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new(OBJECT_STORE_IMAGE, OBJECT_STORE_TAG)
        .with_exposed_port(9000.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Starting:"))
        .with_env_var("RUSTFS_ACCESS_KEY", ACCESS_KEY)
        .with_env_var("RUSTFS_SECRET_KEY", SECRET_KEY)
        .start()
        .await
        .expect("start object store");
    let port = container
        .get_host_port_ipv4(9000)
        .await
        .expect("object store port");
    let endpoint = format!("http://127.0.0.1:{port}");

    let health = format!("{endpoint}/health/ready");
    let client = reqwest::Client::new();
    for _ in 0..120 {
        if let Ok(response) = client.get(&health).send().await {
            if response.status().is_success() {
                return (container, endpoint);
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("object store at {endpoint} did not become ready");
}

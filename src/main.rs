use matrix_sdk::{
    SqliteCryptoStore, SqliteStateStore,
    config::StoreConfig,
    config::SyncSettings,
    ruma::{room_id, user_id},
};
use std::fs;
use std::path::{Path, PathBuf};
use tracing_subscriber::{
    Layer,
    filter::{EnvFilter, LevelFilter},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

async fn get_store_config(path: impl AsRef<Path>, locks_name: String) -> StoreConfig {
    let state_store = SqliteStateStore::open(path.as_ref().to_path_buf(), None)
        .await
        .unwrap();

    let crypto_store = SqliteCryptoStore::open(path.as_ref().to_path_buf(), None)
        .await
        .unwrap();

    StoreConfig::new(locks_name)
        .state_store(state_store)
        .crypto_store(crypto_store)
}

fn log_file(log_name: &str) -> fs::File {
    let log_dir = Path::new("logs");
    let file_path = log_dir.join(format!("{}.log", log_name));

    std::fs::create_dir_all(log_dir).expect("Failed to create log directory");
    std::fs::File::create(file_path).expect("Failed to create log file")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let log_name = uuid::Uuid::new_v4().to_string();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(log_file(&log_name))
        .with_level(true)
        .with_ansi(false)
        .with_filter(EnvFilter::new("matrix_room_redact=debug"));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_level(true)
        .with_filter(LevelFilter::INFO);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    tracing::info!("Log file: {}", log_name);

    let store_config = get_store_config(
        PathBuf::from("state/tmp/matrix-sdk-state"),
        String::from("store-lock"),
    )
    .await;

    let user = user_id!("@user:matrix.org");
    let client = matrix_sdk::Client::builder()
        .server_name(user.server_name())
        .store_config(store_config)
        .build()
        .await?;

    client
        .matrix_auth()
        .login_username(user, "password")
        .initial_device_display_name("SDK")
        .device_id("existing-device-id")
        .send()
        .await?;

    client
        .encryption()
        .import_room_keys(PathBuf::from("element-keys.txt"), "passphrase")
        .await
        .unwrap();

    // Start syncing in background to receive encryption keys
    tokio::spawn({
        let client = client.clone();
        async move {
            client
                .sync(SyncSettings::default())
                .await
                .expect("Sync failed");
        }
    });

    tracing::info!("Sleeping for 15 seconds to allow for sync...");
    tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;

    let room_id = room_id!("!your-internal-room-id:matrix.org");

    if let Some(room) = client.get_room(room_id) {
        tracing::info!("Starting redaction process for room: {}", room_id);

        let reason: Option<String> = None;
        match matrix_room_redact::redact_history(room, reason).await {
            Ok(_) => {
                tracing::info!(
                    "Successfully completed redaction process for room {}",
                    room_id
                );
            }
            Err(e) => {
                tracing::error!("Redaction process failed for room {}: {}", room_id, e);
            }
        }
    } else {
        tracing::error!("Room {} not found or client is not joined.", room_id);
    }

    client.logout().await?;
    Ok(())
}

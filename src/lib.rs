use matrix_sdk::{
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    room::{MessagesOptions, Room},
    ruma::{
        OwnedEventId,
        api::client::filter::RoomEventFilter,
        events::{AnyMessageLikeEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent},
    },
};
use std::sync::Arc;
use tokio::{
    sync::Semaphore,
    sync::mpsc::{self, Receiver, Sender},
    task::JoinSet,
};

#[derive(Debug, thiserror::Error)]
pub enum RedactHistoryError {
    #[error("failed fetching messages")]
    FetchMessages(#[source] matrix_sdk::Error),
    #[error("failed redacting messages")]
    RedactMessages(#[source] matrix_sdk::Error),
    #[error("failed joining tasks")]
    Join,
}

fn get_room_filter() -> RoomEventFilter {
    // If we are continuing from previous run we don't want already redacted events.
    let mut filter = RoomEventFilter::default();
    filter.not_types = vec!["m.room.redaction".into()];
    filter
}

fn get_options(next_token: Option<String>) -> MessagesOptions {
    let mut options = MessagesOptions::backward();
    options.limit = 50u8.into();
    options.filter = get_room_filter();

    match &next_token {
        Some(token) => {
            tracing::info!(token, "Fetching next message chunk");
            options.from(Some(token.as_ref()))
        }
        None => {
            tracing::info!("Fetching initial message chunk");
            options
        }
    }
}

fn is_message(event: &TimelineEvent) -> bool {
    match &event.kind {
        TimelineEventKind::Decrypted(decrypted) => {
            let parsed = match decrypted.event.deserialize() {
                Ok(deserialized) => deserialized,
                Err(e) => {
                    tracing::error!("Failed to deserialise decrypted event: {}", e);
                    return false;
                }
            };

            matches!(parsed, AnyMessageLikeEvent::RoomMessage(..))
        }
        TimelineEventKind::PlainText { event } => {
            let parsed = match event.deserialize() {
                Ok(deserialized) => deserialized,
                Err(e) => {
                    tracing::error!("Failed to deserialise plaintext event: {}", e);
                    return false;
                }
            };

            matches!(
                parsed,
                AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(..))
            )
        }
        TimelineEventKind::UnableToDecrypt { event, utd_info } => {
            tracing::error!("Unable to decrypt: {:?}", utd_info);
            let parsed = match event.deserialize() {
                Ok(deserialized) => deserialized,
                Err(e) => {
                    tracing::error!("Failed to deserialise UTD event: {}", e);
                    return false;
                }
            };

            matches!(
                parsed,
                AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(..))
            )
        }
    }
}

async fn fetch_events(room: Arc<Room>, tx: Sender<OwnedEventId>) -> matrix_sdk::Result<()> {
    let mut next_token: Option<String> = None;
    let (mut idx, mut total_events) = (1, 0);

    loop {
        let options = get_options(next_token);
        let messages = room.messages(options).await?;

        let events = messages.chunk;
        let events_size = events.len();
        tracing::info!(
            idx,
            total_events,
            "Number of events in chunk: {}",
            events_size
        );

        if events.is_empty() {
            tracing::info!("Received empty chunk, assuming end of history");
            break;
        }

        for event in events {
            if !is_message(&event) {
                continue;
            }

            if let Some(event_id) = event.event_id() {
                if tx.send(event_id).await.is_err() {
                    tracing::info!("Receiver dropped, stopping message fetching");
                    return Ok(());
                }
            } else {
                tracing::warn!("Timeline item found without an event ID, skipping");
            }
        }

        next_token = messages.end;
        if next_token.is_none() {
            tracing::info!("Reached end of timeline history (no further backward token)");
            break;
        }

        idx += 1;
        total_events += events_size;

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }

    Ok(())
}

async fn redact_events(
    room: Arc<Room>,
    mut rx: Receiver<OwnedEventId>,
    reason: Option<String>,
) -> matrix_sdk::Result<()> {
    let mut tasks = JoinSet::new();
    let reason = Arc::new(reason);
    let semaphore = Arc::new(Semaphore::new(5));

    while let Some(event_id) = rx.recv().await {
        let room = Arc::clone(&room);
        let reason = Arc::clone(&reason);
        let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();

        tasks.spawn(async move {
            match room.redact(&event_id, reason.as_deref(), None).await {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("Failed to redact: {}", e);
                }
            }

            drop(permit);
        });
    }

    tracing::info!("Event channel closed, waiting for outstanding redaction tasks to complete...");

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(_) => {}
            Err(e) => {
                tracing::error!("A redaction task failed to complete: {}", e);
            }
        }
    }

    Ok(())
}

pub async fn redact_history(room: Room, reason: Option<String>) -> Result<(), RedactHistoryError> {
    let room = Arc::new(room);
    let (tx, rx) = mpsc::channel(100);

    let fetch_room = Arc::clone(&room);
    let fetch_handle = tokio::spawn(async move {
        match fetch_events(fetch_room, tx).await {
            Ok(_) => {
                tracing::info!("Finished fetching all message event IDs.");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Error occurred during message fetching: {}", e);
                Err(RedactHistoryError::FetchMessages(e))
            }
        }
    });

    let redact_room = Arc::clone(&room);
    let redact_handle = tokio::spawn(async move {
        match redact_events(redact_room, rx, reason).await {
            Ok(_) => {
                tracing::info!("Completed all redaction tasks.");
                Ok(())
            }
            Err(e) => {
                tracing::error!("Error occurred during event redaction processing: {}", e);
                Err(RedactHistoryError::RedactMessages(e))
            }
        }
    });

    match tokio::try_join!(fetch_handle, redact_handle) {
        Ok(_) => {
            tracing::info!("Fetching and redaction tasks completed successfully.");
            Ok(())
        }
        Err(e) => {
            tracing::error!("A task failed: {}", e);
            Err(RedactHistoryError::Join)
        }
    }
}

# matrix-room-redact

Quick &amp; Dirty Matrix Room Events Redaction

Uses the Rust Matrix SDK to redact all events in a room.

This is a quick and dirty implementation and requires the E2EE keys to be imported.

Note: If compiling fails due to sqlite, try swapping the matrix-sdk features from `sqlite` to `bundled-sqlite`.

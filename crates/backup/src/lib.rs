//! Encrypted, streaming backups, shared by the control plane (its own
//! database) and modula-node (service databases).
//!
//! A dump's plaintext is age-encrypted as it streams out of the dump command
//! and uploaded to S3-compatible storage part by part through presigned URLs,
//! so whoever runs the dump never needs storage credentials, and the whole
//! dump is never buffered in memory or written to disk.

pub mod keys;
pub mod net;
pub mod s3;
pub mod stream;

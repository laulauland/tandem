//! Content request/response types for lazy blob loading

use serde::{Deserialize, Serialize};

/// Request for lazy content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentRequest {
    pub hash: String,
    pub state_vector: Vec<u8>,
}

/// Response with content update
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentResponse {
    pub hash: String,
    pub update: Vec<u8>,
}

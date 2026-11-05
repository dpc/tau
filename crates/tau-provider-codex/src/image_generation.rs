//! Bounded, single-attempt Codex Images transport; never an API-key fallback.

use std::io::Cursor;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use tokio::runtime;
use tokio::sync::watch;

use crate::ResolvedCredentials;

mod input_image;
pub use input_image::{InputImage, MAX_ATTACHMENTS, MAX_INPUT_BYTES};
use input_image::{MAX_DATA_URL_BYTES, MAX_REQUEST_BYTES};

/// Maximum original PNG accepted by shared harness artifact storage.
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum prompt accepted by the generation/editing tool.
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// Absolute network deadline; timeout does not prove remote work stopped.
pub const GENERATION_TIMEOUT: Duration = Duration::from_secs(300);
/// Fixed subscription backend endpoint, separate from ordinary Responses.
const GENERATION_URL: &str = "https://chatgpt.com/backend-api/codex/images/generations";
/// Sibling JSON edit route, with the same selected-account authority.
const EDIT_URL: &str = "https://chatgpt.com/backend-api/codex/images/edits";
/// Bounds the JSON/base64 response before decoding it.
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Latched cancellation shared with a running generation, without polling.
pub struct GenerationCancellation(watch::Sender<bool>);

impl Default for GenerationCancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl GenerationCancellation {
    /// Wakes a waiting network operation and prevents a future operation.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Checks the latched state before issuing a request.
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Subscribing observes cancellation even when it preceded this waiter.
    async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

/// Byte-free failure classes safe to expose through an ordinary tool error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationError {
    /// The prompt is empty or exceeds the local byte bound.
    InvalidPrompt,
    /// An attachment is unsupported, animated, malformed or beyond decode
    /// limits.
    InvalidInput,
    /// Local request/runtime setup failed before generation could start.
    Setup,
    /// The selected account could not authenticate or is not entitled.
    Authentication,
    /// The endpoint refused this request without returning an image.
    Refused,
    /// The selected account hit a quota or rate limit.
    Quota,
    /// Local transport failed; the remote generation outcome is unknown.
    Transport,
    /// Local cancellation stopped waiting; remote generation may continue.
    Cancelled,
    /// The absolute request deadline expired; remote generation may continue.
    Timeout,
    /// The response or original exceeded a local resource limit.
    TooLarge,
    /// The response did not contain exactly one valid original PNG.
    InvalidImage,
}

impl std::fmt::Display for GenerationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPrompt => "prompt must contain 1 through 32768 bytes",
            Self::InvalidInput => {
                "image attachments must be valid static PNG, JPEG or WebP originals"
            }
            Self::Setup => "image generation could not start locally",
            Self::Authentication => {
                "selected image-generation account is unavailable or unauthorized"
            }
            Self::Refused => "provider refused the image-generation request",
            Self::Quota => "selected account image-generation quota or rate limit reached",
            Self::Transport => "image request failed; remote outcome is uncertain; not retried",
            Self::Cancelled => {
                "image request cancelled locally; remote outcome is uncertain; not retried"
            }
            Self::Timeout => "image request timed out; remote outcome is uncertain; not retried",
            Self::TooLarge => {
                "image request or response exceeded the local size limit; not retried"
            }
            Self::InvalidImage => "provider did not return exactly one valid PNG; not retried",
        })
    }
}

impl std::error::Error for GenerationError {}

/// Returns one original PNG using only the supplied selected-account authority.
///
/// Redirects and transport retries are disabled. The caller owns saving and
/// reporting the result; this function neither persists nor logs image content.
pub fn generate(
    credentials: ResolvedCredentials,
    prompt: &str,
    turn_id: &str,
    network: &tau_provider::OutboundNetworkPolicy,
    cancelled: &GenerationCancellation,
) -> Result<Vec<u8>, GenerationError> {
    generate_with_images(credentials, prompt, turn_id, network, cancelled, Vec::new())
}

/// Generates or edits exactly once; nonempty ordered originals select editing.
pub fn generate_with_images(
    credentials: ResolvedCredentials,
    prompt: &str,
    turn_id: &str,
    network: &tau_provider::OutboundNetworkPolicy,
    cancelled: &GenerationCancellation,
    images: Vec<InputImage>,
) -> Result<Vec<u8>, GenerationError> {
    let endpoint = if images.is_empty() {
        GENERATION_URL
    } else {
        EDIT_URL
    };
    generate_images_at(
        credentials,
        prompt,
        turn_id,
        network,
        cancelled,
        images,
        endpoint,
        GENERATION_TIMEOUT,
    )
}

/// Runs the production transport against an explicit endpoint for local fakes.
#[cfg(test)]
fn generate_at(
    credentials: ResolvedCredentials,
    prompt: &str,
    turn_id: &str,
    network: &tau_provider::OutboundNetworkPolicy,
    cancelled: &GenerationCancellation,
    endpoint: &str,
    timeout: Duration,
) -> Result<Vec<u8>, GenerationError> {
    generate_images_at(
        credentials,
        prompt,
        turn_id,
        network,
        cancelled,
        Vec::new(),
        endpoint,
        timeout,
    )
}

/// Prepares bounded JSON before entering the single-attempt network operation.
#[allow(clippy::too_many_arguments)]
fn generate_images_at(
    credentials: ResolvedCredentials,
    prompt: &str,
    turn_id: &str,
    network: &tau_provider::OutboundNetworkPolicy,
    cancelled: &GenerationCancellation,
    images: Vec<InputImage>,
    endpoint: &str,
    timeout: Duration,
) -> Result<Vec<u8>, GenerationError> {
    if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(GenerationError::InvalidPrompt);
    }
    if cancelled.is_cancelled() {
        return Err(GenerationError::Cancelled);
    }
    let body = request_body(prompt, images, cancelled)?;
    if cancelled.is_cancelled() {
        return Err(GenerationError::Cancelled);
    }
    let runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| GenerationError::Setup)?;
    let client = network
        .client_for_without_retries(endpoint)
        .map_err(|_| GenerationError::Setup)?;
    runtime.block_on(async {
        let operation = request_image(client, credentials, body, turn_id, endpoint);
        tokio::select! {
            biased;
            () = cancelled.cancelled() => Err(GenerationError::Cancelled),
            result = tokio::time::timeout(timeout, operation) => {
                result.map_err(|_| GenerationError::Timeout)?
            }
        }
    })
}

/// Minimal request preserving the generation shape when no inputs were
/// supplied.
#[derive(Serialize)]
struct ImageRequest<'a> {
    /// Fixed subscription image model.
    model: &'static str,
    /// User instruction, with no additional provider controls.
    prompt: &'a str,
    /// Exactly one output, also for editing.
    n: u8,
    /// Ordered occurrences, omitted entirely for generation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    images: Vec<EditImage>,
}

/// Backend-owned data URL, never an arbitrary URL accepted from tool arguments.
#[derive(Serialize)]
struct EditImage {
    /// Validated original encoded with its decoded media type.
    image_url: String,
}

/// Checks padded base64 expansion before allocating each data URL.
fn data_url_size(bytes: usize, mime: &str) -> Result<usize, GenerationError> {
    bytes
        .checked_add(2)
        .and_then(|n| n.checked_div(3))
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| n.checked_add("data:;base64,".len() + mime.len()))
        .ok_or(GenerationError::TooLarge)
}

/// Releases each source after encoding; at most one decoded raster ever exists.
fn request_body(
    prompt: &str,
    originals: Vec<InputImage>,
    cancelled: &GenerationCancellation,
) -> Result<Vec<u8>, GenerationError> {
    if originals.len() > MAX_ATTACHMENTS {
        return Err(GenerationError::TooLarge);
    }
    let mut raw = 0usize;
    let mut encoded = 0usize;
    let mut images = Vec::with_capacity(originals.len());
    for original in originals {
        if cancelled.is_cancelled() {
            return Err(GenerationError::Cancelled);
        }
        raw = raw
            .checked_add(original.bytes.len())
            .ok_or(GenerationError::TooLarge)?;
        encoded = encoded
            .checked_add(data_url_size(original.bytes.len(), original.mime)?)
            .ok_or(GenerationError::TooLarge)?;
        if MAX_INPUT_BYTES < raw || MAX_DATA_URL_BYTES < encoded {
            return Err(GenerationError::TooLarge);
        }
        let mut image_url = format!("data:{};base64,", original.mime);
        STANDARD.encode_string(&original.bytes, &mut image_url);
        images.push(EditImage { image_url });
    }
    let body = serde_json::to_vec(&ImageRequest {
        model: "gpt-image-2",
        prompt,
        n: 1,
        images,
    })
    .map_err(|_| GenerationError::Setup)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(GenerationError::TooLarge);
    }
    Ok(body)
}

/// Submits exactly one request and bounds its response before PNG validation.
async fn request_image(
    client: reqwest::Client,
    credentials: ResolvedCredentials,
    body: Vec<u8>,
    turn_id: &str,
    endpoint: &str,
) -> Result<Vec<u8>, GenerationError> {
    let mut request = client
        .post(endpoint)
        .header(
            "Authorization",
            format!("Bearer {}", credentials.access_token),
        )
        .header("Accept", "application/json")
        .header("originator", "tau")
        .header("x-codex-image-turn-id", turn_id)
        .header("Content-Type", "application/json")
        .body(body);
    if let Some(account_id) = credentials.account_id {
        request = request.header("chatgpt-account-id", account_id);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| GenerationError::Transport)?;
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err(GenerationError::Authentication),
        429 => return Err(GenerationError::Quota),
        400 | 422 => return Err(GenerationError::Refused),
        _ => return Err(GenerationError::Transport),
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(GenerationError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| GenerationError::Transport)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(GenerationError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    decode_original(&body)
}

/// Minimal response projection: remote metadata and prose never become output.
#[derive(Deserialize)]
struct ImageResponse {
    /// Exactly one original is required; batches are unsupported.
    data: Vec<ImageData>,
}

/// One base64 original from the endpoint's ordinary completed response.
#[derive(Deserialize)]
struct ImageData {
    /// Original bytes, never rendered or logged as text.
    b64_json: String,
}

/// Validates a PNG without re-encoding or changing alpha or metadata.
fn decode_original(body: &[u8]) -> Result<Vec<u8>, GenerationError> {
    let response: ImageResponse =
        serde_json::from_slice(body).map_err(|_| GenerationError::InvalidImage)?;
    let [image] = response.data.as_slice() else {
        return Err(GenerationError::InvalidImage);
    };
    if image.b64_json.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(GenerationError::TooLarge);
    }
    let original = STANDARD
        .decode(&image.b64_json)
        .map_err(|_| GenerationError::InvalidImage)?;
    if original.len() > MAX_IMAGE_BYTES {
        return Err(GenerationError::TooLarge);
    }
    let mut reader =
        image::ImageReader::with_format(Cursor::new(&original), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|_| GenerationError::InvalidImage)?;
    Ok(original)
}

#[cfg(test)]
mod tests;

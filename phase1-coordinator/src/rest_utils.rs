// Utility types for the rest API

use crate::{
    authentication::{Production, Signature},
    objects::Task,
    s3::{S3Ctx, S3Error},
    storage::{ContributionLocator, ContributionSignatureLocator},
    CoordinatorError,
    Participant,
};

pub use crate::{
    coordinator_state::{PRIVATE_TOKEN_PREFIX, TOKENS_PATH},
    s3::TOKENS_ZIP_FILE,
};
use blake2::Digest;
use rocket::{
    catch,
    data::FromData,
    error,
    http::{ContentType, Status},
    request::{FromRequest, Outcome, Request},
    response::{Responder, Response},
    serde::{Deserialize, DeserializeOwned, Serialize},
    tokio::{sync::RwLock, task},
    State,
};

use anyhow::anyhow;

use sha2::Sha256;
use subtle::ConstantTimeEq;

use lazy_static::lazy_static;
use std::{borrow::Cow, convert::TryFrom, io::Cursor, net::IpAddr, ops::Deref, sync::Arc, time::Duration};
use thiserror::Error;
use tracing::warn;

#[cfg(debug_assertions)]
pub const UPDATE_TIME: Duration = Duration::from_secs(5);
#[cfg(not(debug_assertions))]
pub const UPDATE_TIME: Duration = Duration::from_secs(60);

pub const UNKNOWN: &str = "Unknown";
pub const TOKEN_REGEX: &str = r"^[A-HJ-NP-Za-km-z1-9]{115}$";

// Headers
pub const BODY_DIGEST_HEADER: &str = "Digest";
pub const PUBKEY_HEADER: &str = "ATS-Pubkey";
pub const SIGNATURE_HEADER: &str = "ATS-Signature";
pub const CONTENT_LENGTH_HEADER: &str = "Content-Length";
pub const ACCESS_SECRET_HEADER: &str = "Access-Secret";

lazy_static! {
    pub(crate) static ref HEALTH_PATH: String = match std::env::var("HEALTH_PATH") {
        Ok(path) => path,
        Err(_) => "./health.json".to_string(),
    };
    pub(crate) static ref ACCESS_SECRET: String =
        std::env::var("ACCESS_SECRET").expect("Missing required env ACCESS_SECRET");
    static ref TOKEN_BLACKLIST: bool = match std::env::var("TOKEN_BLACKLIST") {
        Ok(s) if s == "true" => true,
        _ => false,
    };
}

pub(crate) type Coordinator = Arc<RwLock<crate::Coordinator>>;

/// Server errors. Also includes errors generated by the managed [Coordinator](`crate::Coordinator`).
#[derive(Error, Debug)]
pub enum ResponseError {
    #[error("Ceremony is over, no more contributions are allowed")]
    CeremonyIsOver,
    #[error("Coordinator failed: {0}")]
    CoordinatorError(CoordinatorError),
    #[error("Contribution info is not valid: {0}")]
    InvalidContributionInfo(String),
    #[error("The required access secret is either missing or invalid")]
    InvalidSecret,
    #[error("Header {0} is badly formatted")]
    InvalidHeader(&'static str),
    #[error("Updated tokens for current cohort don't match the old ones")]
    InvalidNewTokens,
    #[error("Request's signature is invalid")]
    InvalidSignature,
    #[error("Authentification token for cohort {0} is invalid")]
    InvalidToken(usize),
    #[error("Io Error: {0}")]
    IoError(String),
    #[error("Checksum of body doesn't match the expected one: expc {0}, act: {1}")]
    MismatchingChecksum(String, String),
    #[error("The required {0} header was missing from the incoming request")]
    MissingRequiredHeader(&'static str),
    #[error("Couldn't verify signature because of missing signing key")]
    MissingSigningKey,
    #[error("Couldn't parse string to int: {0}")]
    ParseError(#[from] std::num::ParseIntError),
    #[error("Thread panicked: {0}")]
    RuntimeError(#[from] task::JoinError),
    #[error("Error with S3: {0}")]
    S3Error(#[from] S3Error),
    #[error("Error with Serde: {0}")]
    SerdeError(String),
    #[error("Error while terminating the ceremony: {0}")]
    ShutdownError(String),
    #[error("The provided token is currently being used in the ceremony")]
    TokenAlreadyInUse,
    #[error("The provided token has already been used in the ceremony")]
    BlacklistedToken,
    #[error("The participant {0} is not allowed to access the endpoint {1} because of: {2}")]
    UnauthorizedParticipant(Participant, String, String),
    #[error("Could not find contributor with public key {0}")]
    UnknownContributor(String),
    #[error("Could not find the provided Task {0} in coordinator state")]
    UnknownTask(Task),
    #[error("Digest of request's body is not base64 encoded: {0}")]
    WrongDigestEncoding(#[from] base64::DecodeError),
}

impl<'r> Responder<'r, 'static> for ResponseError {
    fn respond_to(self, _request: &'r Request<'_>) -> rocket::response::Result<'static> {
        let response = format!("{}", self);
        let mut builder = Response::build();

        let response_code = match self {
            ResponseError::BlacklistedToken => Status::Unauthorized,
            ResponseError::CeremonyIsOver => Status::Unauthorized,
            ResponseError::InvalidHeader(_) => Status::BadRequest,
            ResponseError::InvalidSecret => Status::Unauthorized,
            ResponseError::InvalidSignature => Status::BadRequest,
            ResponseError::InvalidToken(_) => Status::Unauthorized,
            ResponseError::MismatchingChecksum(_, _) => Status::BadRequest,
            ResponseError::MissingRequiredHeader(h) if h == CONTENT_LENGTH_HEADER => Status::LengthRequired,
            ResponseError::MissingRequiredHeader(_) => Status::BadRequest,
            ResponseError::MissingSigningKey => Status::BadRequest,
            ResponseError::SerdeError(_) => Status::UnprocessableEntity,
            ResponseError::TokenAlreadyInUse => Status::Unauthorized,
            ResponseError::UnauthorizedParticipant(_, _, _) => Status::Unauthorized,
            ResponseError::WrongDigestEncoding(_) => Status::BadRequest,
            _ => Status::InternalServerError,
        };

        builder
            .status(response_code)
            .header(ContentType::Text)
            .sized_body(response.len(), Cursor::new(response))
            .ok()
    }
}

pub type Result<T> = std::result::Result<T, ResponseError>;

// Custom catchers for Request/Data Guards. These remap custom error codes to the standard ones and call the ResponseError Responder to produce the response. The default catcher is mantained for non-custom errors

#[catch(452)]
pub fn invalid_signature() -> ResponseError {
    ResponseError::InvalidSignature
}

#[catch(453)]
pub fn unauthorized(req: &Request) -> ResponseError {
    let participant = req.local_cache(|| Participant::new_contributor(UNKNOWN));
    let (endpoint, cause) = req.local_cache(|| (String::from(UNKNOWN), String::from(UNKNOWN)));

    ResponseError::UnauthorizedParticipant(participant.clone(), endpoint.to_owned(), cause.to_owned())
}

#[catch(454)]
pub fn missing_required_header(req: &Request) -> ResponseError {
    let header = req.local_cache(|| UNKNOWN);
    ResponseError::MissingRequiredHeader(header)
}

#[catch(455)]
pub fn unprocessable_entity(req: &Request) -> ResponseError {
    let message = req.local_cache(|| UNKNOWN.to_string());
    ResponseError::SerdeError(message.to_string())
}

#[catch(456)]
pub fn mismatching_checksum(req: &Request) -> ResponseError {
    let (expected, actual) = req.local_cache(|| (UNKNOWN.to_string(), UNKNOWN.to_string()));
    ResponseError::MismatchingChecksum(expected.to_owned(), actual.to_owned())
}

#[catch(457)]
pub fn invalid_header(req: &Request) -> ResponseError {
    let header = req.local_cache(|| UNKNOWN);
    ResponseError::InvalidHeader(header)
}

#[catch(512)]
pub fn io_error(req: &Request) -> ResponseError {
    let message = req.local_cache(|| UNKNOWN.to_string());
    ResponseError::IoError(message.to_owned())
}

/// Content info
pub struct RequestContent<'a> {
    len: usize,
    digest: Cow<'a, str>,
}

impl<'a> RequestContent<'a> {
    pub fn new<T>(len: usize, digest: T) -> Self
    where
        T: AsRef<[u8]>,
    {
        Self {
            len,
            digest: base64::encode(digest).into(),
        }
    }

    /// Returns struct correctly formatted for the http header
    pub fn to_header(&self) -> (usize, String) {
        (self.len, format!("sha-256={}", self.digest))
    }

    /// Constructs from request's headers
    fn try_from_header(len: &str, digest: &'a str) -> Result<Self> {
        let digest = digest
            .split_once('=')
            .ok_or(ResponseError::InvalidHeader(BODY_DIGEST_HEADER))?
            .1;

        // Check encoding
        base64::decode(digest)?;
        let len = len
            .parse()
            .map_err(|_| ResponseError::InvalidHeader(CONTENT_LENGTH_HEADER))?;

        Ok(Self {
            len,
            digest: digest.into(),
        })
    }
}

/// The headers involved in the signature of the request.
#[derive(Default)]
pub struct SignatureHeaders<'r> {
    pub pubkey: &'r str,
    pub content: Option<RequestContent<'r>>,
    pub signature: Option<Cow<'r, str>>,
}

impl<'r> SignatureHeaders<'r> {
    /// Produces the message on which to compute the signature
    pub fn to_string(&self) -> Cow<'_, str> {
        match &self.content {
            Some(content) => format!("{}{}{}", self.pubkey, content.len, content.digest).into(),
            None => self.pubkey.into(),
        }
    }

    pub fn new(pubkey: &'r str, content: Option<RequestContent<'r>>, signature: Option<Cow<'r, str>>) -> Self {
        Self {
            pubkey,
            content,
            signature,
        }
    }

    fn try_verify_signature(&self) -> Result<bool> {
        match &self.signature {
            Some(sig) => Ok(Production.verify(self.pubkey, &self.to_string(), &sig)),
            None => Err(ResponseError::MissingSigningKey),
        }
    }
}

impl<'r> TryFrom<&'r Request<'_>> for SignatureHeaders<'r> {
    type Error = ResponseError;

    fn try_from(request: &'r Request<'_>) -> std::result::Result<Self, Self::Error> {
        let headers = request.headers();
        let mut body: Option<RequestContent> = None;

        let pubkey = headers
            .get_one(PUBKEY_HEADER)
            .ok_or(ResponseError::InvalidHeader(PUBKEY_HEADER))?;
        let sig = headers
            .get_one(SIGNATURE_HEADER)
            .ok_or(ResponseError::InvalidHeader(SIGNATURE_HEADER))?;

        // If post request, also get the hash of body from header (if any and if base64 encoded)
        if request.method() == rocket::http::Method::Post {
            if let Some(s) = headers.get_one(BODY_DIGEST_HEADER) {
                let content_length = headers
                    .get_one(CONTENT_LENGTH_HEADER)
                    .ok_or(ResponseError::InvalidHeader(CONTENT_LENGTH_HEADER))?;
                let content = RequestContent::try_from_header(content_length, s)?;

                body = Some(content);
            }
        }

        Ok(SignatureHeaders::new(pubkey, body, Some(sig.into())))
    }
}

trait VerifySignature<'r> {
    // Workaround to implement a single method on a foreign type instead of newtype pattern
    fn verify_signature(&'r self) -> Result<&str>;
}

impl<'r> VerifySignature<'r> for Request<'_> {
    /// Check signature of request and return the pubkey of the participant
    fn verify_signature(&'r self) -> Result<&str> {
        let headers = SignatureHeaders::try_from(self)?;

        match headers.try_verify_signature()? {
            true => Ok(headers.pubkey),
            false => Err(ResponseError::InvalidSignature),
        }
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Participant {
    type Error = ResponseError;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        match request.verify_signature() {
            Ok(pubkey) => Outcome::Success(Participant::new_contributor(pubkey)),
            Err(e) => Outcome::Failure((Status::new(452), e)),
        }
    }
}

/// Implements the signature verification on the incoming unknown contributor request via [`FromRequest`].
pub struct NewParticipant {
    pub participant: Participant,
    pub ip_address: Option<IpAddr>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for NewParticipant {
    type Error = ResponseError;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let pubkey = match request.verify_signature() {
            Ok(h) => h,
            Err(e) => return Outcome::Failure((Status::new(452), e)),
        };

        // Check that the signature comes from an unknown contributor
        let coordinator = request
            .guard::<&State<Coordinator>>()
            .await
            .succeeded()
            .expect("Managed state should always be retrievable");
        let participant = Participant::new_contributor(pubkey);
        let ip_address = request.client_ip();

        if let Err(e) = coordinator
            .read()
            .await
            .state()
            .add_to_queue_checks(&participant, ip_address.as_ref())
        {
            // Cache error data for the error catcher
            request.local_cache(|| participant.clone());
            request.local_cache(|| (request.uri().to_string(), e.to_string()));

            return Outcome::Failure((
                Status::new(453),
                ResponseError::UnauthorizedParticipant(participant, request.uri().to_string(), e.to_string()),
            ));
        }

        Outcome::Success(Self {
            participant,
            ip_address,
        })
    }
}

/// Implements the signature verification on the incoming current contributor request via [`FromRequest`].
pub struct CurrentContributor(Participant);

impl Deref for CurrentContributor {
    type Target = Participant;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for CurrentContributor {
    type Error = ResponseError;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let pubkey = match request.verify_signature() {
            Ok(h) => h,
            Err(e) => return Outcome::Failure((Status::new(452), e)),
        };

        // Check that the signature comes from the current contributor by matching the public key
        let coordinator = request
            .guard::<&State<Coordinator>>()
            .await
            .succeeded()
            .expect("Managed state should always be retrievable");
        let participant = Participant::new_contributor(pubkey);

        let read_lock = coordinator.read().await;
        if !read_lock.is_current_contributor(&participant) {
            // Cache error data for the error catcher
            let error_msg = {
                if read_lock.is_banned_participant(&participant) {
                    String::from("Participant has been banned from the ceremony")
                } else if read_lock.is_dropped_participant(&participant) {
                    String::from("Participant has been dropped from the ceremony")
                } else {
                    String::from("Participant is not the current contributor")
                }
            };
            drop(read_lock);

            request.local_cache(|| participant.clone());
            request.local_cache(|| (request.uri().to_string(), error_msg.clone()));

            return Outcome::Failure((
                Status::new(453),
                ResponseError::UnauthorizedParticipant(participant, request.uri().to_string(), error_msg),
            ));
        }

        Outcome::Success(Self(participant))
    }
}

/// Implements the secret token verification on the incoming server request via [`FromRequest`]. Used to restrict access to endpoints only when headers contain the valid secret.
/// Can be used as an alternative to [`ServerAuth`] when the body of the request carries no data (and thus doesn't need a signature on that)
pub struct Secret;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Secret {
    type Error = ResponseError;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        match request.headers().get_one(ACCESS_SECRET_HEADER) {
            Some(secret) if secret.as_bytes().ct_eq(&*ACCESS_SECRET.as_bytes()).into() => Outcome::Success(Self),
            _ => Outcome::Failure((Status::new(401), ResponseError::InvalidSecret)),
        }
    }
}

/// Implements the signature verification on the incoming server request via [`FromRequest`].
pub struct ServerAuth;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ServerAuth {
    type Error = ResponseError;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let pubkey = match request.verify_signature() {
            Ok(h) => h,
            Err(e) => return Outcome::Failure((Status::new(452), e)),
        };

        // Check that the signature comes from the coordinator by matching the default verifier key
        let coordinator = request
            .guard::<&State<Coordinator>>()
            .await
            .succeeded()
            .expect("Managed state should always be retrievable");
        let verifier = Participant::new_verifier(pubkey);

        if verifier != coordinator.read().await.environment().coordinator_verifiers()[0] {
            // Cache error data for the error catcher
            let error_msg = String::from("Not the coordinator's verifier");
            request.local_cache(|| verifier.clone());
            request.local_cache(|| (request.uri().to_string(), error_msg.clone()));

            return Outcome::Failure((
                Status::new(453),
                ResponseError::UnauthorizedParticipant(verifier, request.uri().to_string(), error_msg),
            ));
        }

        Outcome::Success(Self)
    }
}

/// Type to handle lazy deserialization of json encoded inputs.
pub struct LazyJson<T>(pub T);

impl<T> Deref for LazyJson<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> std::ops::DerefMut for LazyJson<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[rocket::async_trait]
impl<'r, T: DeserializeOwned> FromData<'r> for LazyJson<T> {
    type Error = ResponseError;

    async fn from_data(req: &'r Request<'_>, data: rocket::data::Data<'r>) -> rocket::data::Outcome<'r, Self> {
        // Check that digest of body is the expected one
        let headers = req.headers();
        let expected_digest = match headers.get_one(BODY_DIGEST_HEADER) {
            Some(h) => h,
            None => {
                // Cache error data for the error catcher
                req.local_cache(|| BODY_DIGEST_HEADER.to_string());

                return rocket::data::Outcome::Failure((
                    Status::new(454),
                    ResponseError::MissingRequiredHeader(BODY_DIGEST_HEADER),
                ));
            }
        };

        let content_length = match headers.get_one(CONTENT_LENGTH_HEADER) {
            Some(h) => h,
            None => {
                // Cache error data for the error catcher
                req.local_cache(|| CONTENT_LENGTH_HEADER.to_string());

                return rocket::data::Outcome::Failure((
                    Status::new(454),
                    ResponseError::MissingRequiredHeader(CONTENT_LENGTH_HEADER),
                ));
            }
        };

        let expected_content = match RequestContent::try_from_header(content_length, expected_digest) {
            Ok(c) => c,
            Err(e) => {
                // Cache error data for the error catcher
                let header = match e {
                    ResponseError::InvalidHeader(h) => h,
                    _ => UNKNOWN,
                };
                req.local_cache(|| header);

                return rocket::data::Outcome::Failure((Status::new(457), e));
            }
        };

        let body = match data.open(expected_content.len.into()).into_bytes().await {
            Ok(bytes) => bytes.into_inner(),
            Err(e) => {
                // Cache error data for the error catcher
                req.local_cache(|| e.to_string());

                return rocket::data::Outcome::Failure((Status::new(512), ResponseError::IoError(e.to_string())));
            }
        };

        let mut hasher = Sha256::new();
        hasher.update(&body);
        let digest = base64::encode(hasher.finalize());
        if digest != expected_content.digest {
            // Cache error data for the error catcher
            req.local_cache(|| (expected_digest.to_owned(), expected_content.digest.to_string()));

            return rocket::data::Outcome::Failure((
                Status::new(456),
                ResponseError::MismatchingChecksum(expected_digest.to_owned(), expected_content.digest.to_string()),
            ));
        }

        // Deserialize data and pass it to the request handler
        match serde_json::from_slice::<T>(&body) {
            Ok(obj) => rocket::data::Outcome::Success(LazyJson(obj)),
            Err(e) => {
                // Cache error data for the error catcher
                req.local_cache(|| (e.to_string()));
                rocket::data::Outcome::Failure((Status::new(455), ResponseError::SerdeError(e.to_string())))
            }
        }
    }
}

/// The status of the contributor related to the current round.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ContributorStatus {
    Queue(u64, u64),
    Round,
    Finished,
    Banned,
    Other,
}

/// Request to post a [Chunk](`crate::objects::Chunk`).
#[derive(Clone, Deserialize, Serialize)]
pub struct PostChunkRequest {
    pub round_height: u64,
    pub contribution_locator: ContributionLocator,
    pub contribution_signature_locator: ContributionSignatureLocator,
}

impl PostChunkRequest {
    pub fn new(
        round_height: u64,
        contribution_locator: ContributionLocator,
        contribution_signature_locator: ContributionSignatureLocator,
    ) -> Self {
        Self {
            round_height,
            contribution_locator,
            contribution_signature_locator,
        }
    }
}

/// Checks the validity of the token for the ceremony.
/// Returns the current cohort index
pub(crate) async fn token_check(coordinator: Coordinator, token: &str) -> Result<u64> {
    // Check that token is not in use nor blacklisted (only if env is set)
    let read_lock = coordinator.read().await;

    // Check that token is not in use nor blacklisted (only if env is set and token is not FFA)
    if *TOKEN_BLACKLIST && token.starts_with(PRIVATE_TOKEN_PREFIX) {
        if read_lock.state().is_token_in_use(token) {
            return Err(ResponseError::TokenAlreadyInUse);
        }

        if read_lock.state().is_token_blacklisted(token) {
            return Err(ResponseError::BlacklistedToken);
        }
    }

    // Check that the token is correct for the current cohort number
    let cohort = read_lock.state().get_current_cohort_index();
    let tokens = match read_lock.state().tokens(cohort) {
        Some(t) => t,
        None => return Err(ResponseError::CeremonyIsOver),
    };

    if !tokens.contains(token) {
        return Err(ResponseError::InvalidToken(cohort + 1));
    }

    Ok((cohort + 1) as u64)
}

/// Performs the verification of the pending contributions
///
/// # Cancel safety
///
/// https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety
///
/// Because of the use of [`tokio::sync::rwlock::RwLock::write_owned`], which is not cancel safe, and a spawned blocking
/// task, which cannot be cancelled, this function is not cancel safe.
pub async fn perform_verify_chunks(coordinator: Coordinator, s3_ctx: &S3Ctx) -> Result<()> {
    // Get all the pending verifications, loop on each one of them and perform verification
    // Technically, since we don't chunk contributions and we only have one contribution per round, we will always get
    // one pending verification at max.
    let mut write_lock = coordinator.write_owned().await;

    // NOTE: we are going to rely on the single default verifier built in the coordinator itself,
    //  no external verifiers
    let contributions_info = task::spawn_blocking(move || -> Result<Vec<u8>> {
        for (task, _) in write_lock.get_pending_verifications().to_owned() {
            if let Err(e) = write_lock.default_verify(&task) {
                warn!("Error while verifying a contribution: {}. Restarting the round...", e);
                // FIXME: the verify_masp function may panic but the program doesn't shut down because we are executing it on a separate thread. It would be better though to make that function return a Result instead of panicking. Revert of round should be moved inside default_verify

                // Get the participant who produced the contribution
                let finished_contributor = write_lock
                    .state()
                    .current_round_finished_contributors()
                    .unwrap()
                    .first()
                    .unwrap()
                    .clone();

                // Reset the round to prevent a coordinator stall (the corrupted contribution is not automatically dropped)
                write_lock
                    .reset_round()
                    .map_err(|e| ResponseError::CoordinatorError(e))?;

                // Ban the participant who produced the invalid contribution. Must be banned after the reset beacuse one can't ban a finished contributor
                write_lock
                    .ban_participant(&finished_contributor)
                    .map_err(|e| ResponseError::CoordinatorError(e))?;
            }
        }

        write_lock
            .storage()
            .get_contributions_summary()
            .map_err(|e| ResponseError::CoordinatorError(e))
    })
    .await??;

    // Upload json file to S3
    s3_ctx
        .upload_contributions_info(contributions_info)
        .await
        .map_err(|e| ResponseError::CoordinatorError(CoordinatorError::Error(anyhow!(e.to_string()))))
}

/// Performs the update of the [Coordinator](`crate::Coordinator`)
///
/// # Cancel safety
///
/// https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety
///
/// Because of the use of [`tokio::sync::rwlock::RwLock::write_owned`], which is not cancel safe, and a spawned blocking
/// task, which cannot be cancelled, this function is not cancel safe.
pub async fn perform_coordinator_update(coordinator: Coordinator) -> Result<()> {
    let mut write_lock = coordinator.write_owned().await;

    task::spawn_blocking(move || write_lock.update())
        .await?
        .map_err(|e| ResponseError::CoordinatorError(e))
}

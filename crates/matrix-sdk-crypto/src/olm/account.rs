// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::BTreeMap,
    convert::TryInto,
    fmt,
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use matrix_sdk_common::{instant::Instant, locks::Mutex};
use olm_rs::{
    account::{FallbackKey, IdentityKeys, OlmAccount, OneTimeKeys},
    errors::{OlmAccountError, OlmSessionError},
    session::{OlmMessage, PreKeyMessage},
    PicklingMode,
};
use ruma::{
    api::client::r0::keys::{upload_keys, upload_signatures::Request as SignatureUploadRequest},
    encryption::{CrossSigningKey, DeviceKeys},
    events::{
        room::encrypted::{
            EncryptedEventScheme, OlmV1Curve25519AesSha2Content, ToDeviceRoomEncryptedEvent,
        },
        AnyToDeviceEvent, OlmV1Keys,
    },
    serde::{CanonicalJsonValue, Raw},
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, EventEncryptionAlgorithm, RoomId, UInt, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue as RawJsonValue, Value};
use sha2::{Digest, Sha256};
use tracing::{debug, info, trace, warn};

use super::{
    EncryptionSettings, InboundGroupSession, OutboundGroupSession, PrivateCrossSigningIdentity,
    Session,
};
use crate::{
    error::{EventError, OlmResult, SessionCreationError},
    identities::{MasterPubkey, ReadOnlyDevice},
    requests::UploadSigningKeysRequest,
    store::{Changes, Store},
    types::{OneTimeKey, SignedKey},
    utilities::encode,
    CryptoStoreError, OlmError, SignatureError,
};

#[derive(Debug, Clone)]
pub struct Account {
    pub(crate) inner: ReadOnlyAccount,
    pub(crate) store: Store,
}

#[derive(Debug, Clone)]
pub enum SessionType {
    New(Session),
    Existing(Session),
}

impl SessionType {
    #[cfg(test)]
    pub fn session(self) -> Session {
        match self {
            SessionType::New(s) => s,
            SessionType::Existing(s) => s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OlmDecryptionInfo {
    pub sender: Box<UserId>,
    pub session: SessionType,
    pub message_hash: OlmMessageHash,
    pub deserialized_event: Option<AnyToDeviceEvent>,
    pub event: Raw<AnyToDeviceEvent>,
    pub signing_key: String,
    pub sender_key: String,
    pub inbound_group_session: Option<InboundGroupSession>,
}

/// A hash of a successfully decrypted Olm message.
///
/// Can be used to check if a message has been replayed to us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OlmMessageHash {
    /// The curve25519 key of the sender that sent us the Olm message.
    pub sender_key: String,
    /// The hash of the message.
    pub hash: String,
}

impl OlmMessageHash {
    fn new(sender_key: &str, message_type: u8, ciphertext: &str) -> Self {
        let sha = Sha256::new().chain(sender_key).chain(&[message_type]).chain(&ciphertext);
        Self { sender_key: sender_key.to_owned(), hash: encode(sha.finalize().as_slice()) }
    }
}

impl Deref for Account {
    type Target = ReadOnlyAccount;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Account {
    fn parse_message(
        sender_key: &str,
        message_type: UInt,
        ciphertext: String,
    ) -> Result<(OlmMessage, OlmMessageHash), EventError> {
        let message_type: u8 = message_type
            .try_into()
            .map_err(|_| EventError::UnsupportedOlmType(message_type.into()))?;

        let message_hash = OlmMessageHash::new(sender_key, message_type, &ciphertext);
        let message = OlmMessage::from_type_and_ciphertext(message_type.into(), ciphertext)
            .map_err(|_| EventError::UnsupportedOlmType(message_type.into()))?;

        Ok((message, message_hash))
    }

    pub(crate) async fn save(&self) -> Result<(), CryptoStoreError> {
        self.store.save_account(self.inner.clone()).await
    }

    async fn decrypt_olm_v1(
        &self,
        sender: &UserId,
        content: &OlmV1Curve25519AesSha2Content,
    ) -> OlmResult<OlmDecryptionInfo> {
        let identity_keys = self.inner.identity_keys();

        // Try to find a ciphertext that was meant for our device.
        if let Some(ciphertext) = content.ciphertext.get(identity_keys.curve25519()) {
            let (message, message_hash) = match Self::parse_message(
                &content.sender_key,
                ciphertext.message_type,
                ciphertext.body.clone(),
            ) {
                Ok(m) => m,
                Err(e) => {
                    warn!(error =? e, "Encrypted to-device event isn't valid");
                    return Err(e.into());
                }
            };

            // Decrypt the OlmMessage and get a Ruma event out of it.
            match self.decrypt_olm_message(sender, &content.sender_key, message).await {
                Ok((session, event, signing_key)) => Ok(OlmDecryptionInfo {
                    sender: sender.to_owned(),
                    session,
                    message_hash,
                    event,
                    signing_key,
                    sender_key: content.sender_key.clone(),
                    deserialized_event: None,
                    inbound_group_session: None,
                }),
                Err(OlmError::SessionWedged(user_id, sender_key)) => {
                    if self.store.is_message_known(&message_hash).await? {
                        info!(
                            sender = sender.as_str(),
                            sender_key = content.sender_key.as_str(),
                            "An Olm message got replayed, decryption failed"
                        );

                        Err(OlmError::ReplayedMessage(user_id, sender_key))
                    } else {
                        Err(OlmError::SessionWedged(user_id, sender_key))
                    }
                }
                Err(e) => Err(e),
            }
        } else {
            warn!(
                sender = sender.as_str(),
                sender_key = content.sender_key.as_str(),
                "Olm event doesn't contain a ciphertext for our key"
            );

            Err(EventError::MissingCiphertext.into())
        }
    }

    pub async fn decrypt_to_device_event(
        &self,
        event: &ToDeviceRoomEncryptedEvent,
    ) -> OlmResult<OlmDecryptionInfo> {
        trace!(sender = event.sender.as_str(), "Decrypting a to-device event");

        if let EncryptedEventScheme::OlmV1Curve25519AesSha2(c) = &event.content.scheme {
            self.decrypt_olm_v1(&event.sender, c).await
        } else {
            warn!(
                sender = event.sender.as_str(),
                algorithm =? event.content.scheme,
                "Error, unsupported encryption algorithm"
            );

            Err(EventError::UnsupportedAlgorithm.into())
        }
    }

    pub async fn receive_keys_upload_response(
        &self,
        response: &upload_keys::Response,
    ) -> OlmResult<()> {
        if !self.inner.shared() {
            debug!("Marking account as shared");
        }
        self.inner.mark_as_shared();

        debug!("Marking one-time keys and fallback key as published");
        self.update_key_counts(&response.one_time_key_counts, None).await;
        self.inner.mark_keys_as_published().await;
        self.store.save_account(self.inner.clone()).await?;

        Ok(())
    }

    /// Try to decrypt an Olm message.
    ///
    /// This try to decrypt an Olm message using all the sessions we share
    /// with the given sender.
    async fn decrypt_with_existing_sessions(
        &self,
        sender: &UserId,
        sender_key: &str,
        message: &OlmMessage,
    ) -> OlmResult<Option<(Session, String)>> {
        let s = self.store.get_sessions(sender_key).await?;

        // We don't have any existing sessions, return early.
        let sessions = if let Some(s) = s {
            s
        } else {
            return Ok(None);
        };

        let mut decrypted: Option<(Session, String)> = None;

        for session in &mut *sessions.lock().await {
            let mut matches = false;

            // If this is a pre-key message check if it was encrypted for our
            // session, if it wasn't decryption will fail so no need to try.
            if let OlmMessage::PreKey(m) = &message {
                matches = session.matches(sender_key, m.clone()).await?;

                if !matches {
                    continue;
                }
            }

            let ret = session.decrypt(message.clone()).await;

            match ret {
                Ok(p) => {
                    decrypted = Some((session.clone(), p));
                    break;
                }
                Err(e) => {
                    // Decryption failed with a matching session, the session is
                    // likely wedged and needs to be rotated.
                    if matches {
                        warn!(
                            sender = sender.as_str(),
                            sender_key = sender_key,
                            error =? e,
                            "Found a matching Olm session yet decryption failed",
                        );
                        return Err(OlmError::SessionWedged(
                            sender.to_owned(),
                            sender_key.to_owned(),
                        ));
                    }
                }
            }
        }

        Ok(decrypted)
    }

    /// Decrypt an Olm message, creating a new Olm session if possible.
    async fn decrypt_olm_message(
        &self,
        sender: &UserId,
        sender_key: &str,
        message: OlmMessage,
    ) -> OlmResult<(SessionType, Raw<AnyToDeviceEvent>, String)> {
        // First try to decrypt using an existing session.
        let (session, plaintext) = if let Some(d) =
            self.decrypt_with_existing_sessions(sender, sender_key, &message).await?
        {
            // Decryption succeeded, de-structure the session/plaintext out of
            // the Option.
            (SessionType::Existing(d.0), d.1)
        } else {
            // Decryption failed with every known session, let's try to create a
            // new session.
            let mut session = match &message {
                // A new session can only be created using a pre-key message,
                // return with an error if it isn't one.
                OlmMessage::Message(_) => {
                    warn!(
                        sender = sender.as_str(),
                        sender_key = sender_key,
                        "Failed to decrypt a non-pre-key message with all \
                        available sessions",
                    );
                    return Err(OlmError::SessionWedged(sender.to_owned(), sender_key.to_owned()));
                }

                OlmMessage::PreKey(m) => {
                    // Create the new session.
                    let session =
                        match self.inner.create_inbound_session(sender_key, m.clone()).await {
                            Ok(s) => s,
                            Err(e) => {
                                warn!(
                                    sender = sender.as_str(),
                                    sender_key = sender_key,
                                    error =? e,
                                    "Failed to create a new Olm session from a \
                                    prekey message",
                                );
                                return Err(OlmError::SessionWedged(
                                    sender.to_owned(),
                                    sender_key.to_owned(),
                                ));
                            }
                        };

                    session
                }
            };

            // Decrypt our message, this shouldn't fail since we're using a
            // newly created Session.
            let plaintext = session.decrypt(message).await?;

            // We need to add the new session to the session cache, otherwise
            // we might try to create the same session again.
            // TODO separate the session cache from the storage so we only add
            // it to the cache but don't store it.
            let changes = Changes {
                account: Some(self.inner.clone()),
                sessions: vec![session.clone()],
                ..Default::default()
            };
            self.store.save_changes(changes).await?;

            (SessionType::New(session), plaintext)
        };

        trace!(
            sender = sender.as_str(),
            sender_key = sender_key,
            "Successfully decrypted an Olm message"
        );

        match self.parse_decrypted_to_device_event(sender, plaintext) {
            Ok((event, signing_key)) => Ok((session, event, signing_key)),
            Err(e) => {
                // We might created a new session but decryption might still
                // have failed, store it for the error case here, this is fine
                // since we don't expect this to happen often or at all.
                match session {
                    SessionType::New(s) => {
                        let changes = Changes {
                            account: Some(self.inner.clone()),
                            sessions: vec![s],
                            ..Default::default()
                        };
                        self.store.save_changes(changes).await?;
                    }
                    SessionType::Existing(s) => {
                        self.store.save_sessions(&[s]).await?;
                    }
                }

                warn!(
                    sender = sender.as_str(),
                    sender_key = sender_key,
                    error =? e,
                    "A to-device message was successfully decrypted but \
                    parsing and checking the event fields failed"
                );

                Err(e)
            }
        }
    }

    /// Parse a decrypted Olm message, check that the plaintext and encrypted
    /// senders match and that the message was meant for us.
    fn parse_decrypted_to_device_event(
        &self,
        sender: &UserId,
        plaintext: String,
    ) -> OlmResult<(Raw<AnyToDeviceEvent>, String)> {
        #[derive(Deserialize)]
        struct DecryptedEvent {
            sender: Box<UserId>,
            recipient: Box<UserId>,
            recipient_keys: OlmV1Keys,
            keys: OlmV1Keys,
        }

        let event: DecryptedEvent = serde_json::from_str(&plaintext)?;
        let identity_keys = self.inner.identity_keys();

        if event.recipient != self.user_id() {
            Err(EventError::MismatchedSender(event.recipient, self.user_id().to_owned()).into())
        } else if event.sender != sender {
            Err(EventError::MismatchedSender(event.sender, sender.to_owned()).into())
        } else if identity_keys.ed25519() != event.recipient_keys.ed25519 {
            Err(EventError::MismatchedKeys(
                identity_keys.ed25519().to_owned(),
                event.recipient_keys.ed25519,
            )
            .into())
        } else {
            Ok((Raw::from_json(RawJsonValue::from_string(plaintext)?), event.keys.ed25519))
        }
    }
}

/// Account holding identity keys for which sessions can be created.
///
/// An account is the central identity for encrypted communication between two
/// devices.
#[derive(Clone)]
pub struct ReadOnlyAccount {
    pub(crate) user_id: Arc<UserId>,
    pub(crate) device_id: Arc<DeviceId>,
    inner: Arc<Mutex<OlmAccount>>,
    pub(crate) identity_keys: Arc<IdentityKeys>,
    shared: Arc<AtomicBool>,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Arc<AtomicU64>,
}

/// A typed representation of a base64 encoded string containing the account
/// pickle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountPickle(String);

impl AccountPickle {
    /// Get the string representation of the pickle.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for AccountPickle {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A pickled version of an `Account`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledAccount {
    /// The user id of the account owner.
    pub user_id: Box<UserId>,
    /// The device id of the account owner.
    pub device_id: Box<DeviceId>,
    /// The pickled version of the Olm account.
    pub pickle: AccountPickle,
    /// Was the account shared.
    pub shared: bool,
    /// The number of uploaded one-time keys we have on the server.
    pub uploaded_signed_key_count: u64,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for ReadOnlyAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("identity_keys", self.identity_keys())
            .field("shared", &self.shared())
            .finish()
    }
}

impl ReadOnlyAccount {
    const ALGORITHMS: &'static [&'static EventEncryptionAlgorithm] = &[
        &EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
        &EventEncryptionAlgorithm::MegolmV1AesSha2,
    ];

    /// Create a fresh new account, this will generate the identity key-pair.
    #[allow(clippy::ptr_arg)]
    pub fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        let account = OlmAccount::new();
        let identity_keys = account.parsed_identity_keys();

        Self {
            user_id: user_id.into(),
            device_id: device_id.into(),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::new(false)),
            uploaded_signed_key_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn update_key_counts(
        &self,
        one_time_key_counts: &BTreeMap<DeviceKeyAlgorithm, UInt>,
        fallback_key_counts: Option<&[DeviceKeyAlgorithm]>,
    ) {
        if let Some(count) = one_time_key_counts.get(&DeviceKeyAlgorithm::SignedCurve25519) {
            let count: u64 = (*count).into();
            let old_count = self.uploaded_key_count();

            // Some servers might always return the key counts in the sync
            // response, we don't want to the logs with noop changes if they do
            // so.
            if count != old_count {
                debug!(
                    "Updated uploaded one-time key count {} -> {}.",
                    self.uploaded_key_count(),
                    count
                );
            }

            self.update_uploaded_key_count(count);
        }

        if let Some(unused) = fallback_key_counts {
            if !unused.contains(&DeviceKeyAlgorithm::SignedCurve25519) {
                // Generate a new fallback key if we don't have one.
                self.generate_fallback_key_helper().await;
            }
        }
    }

    /// Get the user id of the owner of the account.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the device id that owns this account.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> &IdentityKeys {
        &self.identity_keys
    }

    /// Update the uploaded key count.
    ///
    /// # Arguments
    ///
    /// * `new_count` - The new count that was reported by the server.
    pub(crate) fn update_uploaded_key_count(&self, new_count: u64) {
        self.uploaded_signed_key_count.store(new_count, Ordering::SeqCst);
    }

    /// Get the currently known uploaded key count.
    pub fn uploaded_key_count(&self) -> u64 {
        self.uploaded_signed_key_count.load(Ordering::SeqCst)
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
    }

    /// Mark the account as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub(crate) fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::SeqCst);
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub(crate) async fn one_time_keys(&self) -> OneTimeKeys {
        self.inner.lock().await.parsed_one_time_keys()
    }

    async fn fallback_key(&self) -> Option<FallbackKey> {
        self.inner.lock().await.parsed_fallback_key()
    }

    /// Generate count number of one-time keys.
    pub(crate) async fn generate_one_time_keys_helper(&self, count: usize) {
        self.inner.lock().await.generate_one_time_keys(count);
    }

    async fn generate_fallback_key_helper(&self) {
        let account = self.inner.lock().await;

        if account.parsed_fallback_key().is_none() {
            debug!(
                "No unused fallback keys were found on the server, generating \
                a new fallback key.",
            );

            account.generate_fallback_key();
        }
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub(crate) async fn max_one_time_keys(&self) -> usize {
        self.inner.lock().await.max_number_of_one_time_keys()
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub(crate) async fn generate_one_time_keys(&self) -> Result<u64, ()> {
        // Only generate one-time keys if there aren't any, otherwise the caller
        // might have failed to upload them the last time this method was
        // called.
        if self.one_time_keys().await.curve25519().is_empty() {
            let count = self.uploaded_key_count();
            let max_keys = self.max_one_time_keys().await;
            let max_on_server = (max_keys as u64) / 2;

            if count >= (max_on_server) {
                return Err(());
            }

            let key_count = max_on_server - count;
            let key_count: usize = key_count.try_into().unwrap_or(max_keys);

            self.generate_one_time_keys_helper(key_count).await;
            Ok(key_count as u64)
        } else {
            Ok(0)
        }
    }

    /// Should account or one-time keys be uploaded to the server.
    pub(crate) async fn should_upload_keys(&self) -> bool {
        if !self.shared() || self.fallback_key().await.is_some() {
            true
        } else {
            let count = self.uploaded_key_count();

            // If we have a known key count, check that we have more than
            // max_one_time_Keys() / 2, otherwise tell the client to upload more.
            let max_keys = self.max_one_time_keys().await as u64;
            // If there are more keys already uploaded than max_key / 2
            // bail out returning false, this also avoids overflow.
            if count > (max_keys / 2) {
                return false;
            }

            let key_count = (max_keys / 2) - count;

            key_count > 0
        }
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns None if no keys need to be uploaded.
    pub(crate) async fn keys_for_upload(
        &self,
    ) -> Option<(
        Option<DeviceKeys>,
        BTreeMap<Box<DeviceKeyId>, Raw<ruma::encryption::OneTimeKey>>,
        BTreeMap<Box<DeviceKeyId>, Raw<ruma::encryption::OneTimeKey>>,
    )> {
        if !self.should_upload_keys().await {
            return None;
        }

        let device_keys = if !self.shared() { Some(self.device_keys().await) } else { None };
        let one_time_keys = self.signed_one_time_keys().await.ok().unwrap_or_default();
        let fallback_keys = self.signed_fallback_keys().await;

        Some((device_keys, one_time_keys, fallback_keys))
    }

    /// Mark the current set of one-time keys as being published.
    pub(crate) async fn mark_keys_as_published(&self) {
        self.inner.lock().await.mark_keys_as_published();
    }

    /// Sign the given string using the accounts signing key.
    ///
    /// Returns the signature as a base64 encoded string.
    pub async fn sign(&self, string: &str) -> String {
        self.inner.lock().await.sign(string)
    }

    #[cfg(feature = "backups_v1")]
    pub(crate) fn is_signed(&self, json: &mut Value) -> Result<(), SignatureError> {
        let signing_key = self.identity_keys.ed25519();
        let utility = crate::olm::Utility::new();

        utility.verify_json(
            &self.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signing_key,
            json,
        )
    }

    /// Store the account as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either
    ///   an
    /// unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> PickledAccount {
        let pickle = AccountPickle(self.inner.lock().await.pickle(pickle_mode));

        PickledAccount {
            user_id: self.user_id().to_owned(),
            device_id: self.device_id().to_owned(),
            pickle,
            shared: self.shared(),
            uploaded_signed_key_count: self.uploaded_key_count(),
        }
    }

    /// Restore an account from a previously pickled one.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the Account.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either
    ///   an
    /// unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(
        pickle: PickledAccount,
        pickle_mode: PicklingMode,
    ) -> Result<Self, OlmAccountError> {
        let account = OlmAccount::unpickle(pickle.pickle.0, pickle_mode)?;
        let identity_keys = account.parsed_identity_keys();

        Ok(Self {
            user_id: pickle.user_id.into(),
            device_id: pickle.device_id.into(),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::from(pickle.shared)),
            uploaded_signed_key_count: Arc::new(AtomicU64::new(pickle.uploaded_signed_key_count)),
        })
    }

    pub(crate) fn unsigned_device_keys(&self) -> DeviceKeys {
        let identity_keys = self.identity_keys();
        let keys = BTreeMap::from([
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Curve25519, &self.device_id),
                identity_keys.curve25519().to_owned(),
            ),
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                identity_keys.ed25519().to_owned(),
            ),
        ]);

        DeviceKeys::new(
            (*self.user_id).to_owned(),
            (*self.device_id).to_owned(),
            Self::ALGORITHMS.iter().map(|a| (&**a).clone()).collect(),
            keys,
            BTreeMap::new(),
        )
    }

    /// Sign the device keys of the account and return them so they can be
    /// uploaded.
    pub(crate) async fn device_keys(&self) -> DeviceKeys {
        let mut device_keys = self.unsigned_device_keys();

        // Create a copy of the device keys containing only fields that will
        // get signed.
        let json_device_keys = json!({
            "user_id": device_keys.user_id,
            "device_id": device_keys.device_id,
            "algorithms": device_keys.algorithms,
            "keys": device_keys.keys,
        });

        device_keys
            .signatures
            .entry(self.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                self.sign_json(json_device_keys).await,
            );

        device_keys
    }

    pub(crate) async fn bootstrap_cross_signing(
        &self,
    ) -> (PrivateCrossSigningIdentity, UploadSigningKeysRequest, SignatureUploadRequest) {
        PrivateCrossSigningIdentity::new_with_account(self).await
    }

    pub(crate) async fn sign_cross_signing_key(
        &self,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let signature = self.sign_json(serde_json::to_value(&cross_signing_key)?).await;

        cross_signing_key
            .signatures
            .entry(self.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()).to_string(),
                signature,
            );

        Ok(())
    }

    pub(crate) async fn sign_master_key(
        &self,
        master_key: MasterPubkey,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let public_key =
            master_key.get_first_key().ok_or(SignatureError::MissingSigningKey)?.to_string();
        let mut cross_signing_key: CrossSigningKey = master_key.into();
        cross_signing_key.signatures.clear();
        self.sign_cross_signing_key(&mut cross_signing_key).await?;

        let mut signed_keys = BTreeMap::new();
        signed_keys
            .entry(self.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(public_key, serde_json::to_value(cross_signing_key)?);

        Ok(SignatureUploadRequest::new(signed_keys))
    }

    /// Convert a JSON value to the canonical representation and sign the JSON
    /// string.
    ///
    /// # Arguments
    ///
    /// * `json` - The value that should be converted into a canonical JSON
    /// string.
    ///
    /// # Panic
    ///
    /// Panics if the json value can't be serialized.
    pub async fn sign_json(&self, mut json: Value) -> String {
        let object = json.as_object_mut().expect("Canonical json value isn't an object");
        object.remove("unsigned");
        object.remove("signatures");

        let canonical_json: CanonicalJsonValue =
            json.try_into().expect("Can't canonicalize the json value");

        self.sign(&canonical_json.to_string()).await
    }

    async fn signed_fallback_keys(
        &self,
    ) -> BTreeMap<Box<DeviceKeyId>, Raw<ruma::encryption::OneTimeKey>> {
        if let Some(fallback_key) = self.fallback_key().await {
            let mut fallback_key_map = BTreeMap::new();
            let key_id = fallback_key.index();
            let key = fallback_key.curve25519();

            let signed_key = self.sign_key(key, true).await;

            fallback_key_map.insert(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::SignedCurve25519, key_id.into()),
                signed_key.to_raw(),
            );

            fallback_key_map
        } else {
            BTreeMap::new()
        }
    }

    async fn sign_key(&self, key: &str, fallback: bool) -> SignedKey {
        let mut key = if fallback {
            SignedKey::new_fallback(key.to_owned())
        } else {
            SignedKey::new(key.to_owned())
        };

        let signature =
            self.sign_json(serde_json::to_value(&key).expect("Can't serialize a signed key")).await;

        let signatures = BTreeMap::from([(
            self.user_id().to_owned(),
            BTreeMap::from([(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                signature,
            )]),
        )]);

        *key.signatures() = signatures;

        key
    }

    pub(crate) async fn signed_one_time_keys_helper(
        &self,
    ) -> Result<BTreeMap<Box<DeviceKeyId>, Raw<ruma::encryption::OneTimeKey>>, ()> {
        let one_time_keys = self.one_time_keys().await;
        let mut one_time_key_map = BTreeMap::new();

        for (key_id, key) in one_time_keys.curve25519().iter() {
            let signed_key = self.sign_key(key, false).await;

            one_time_key_map.insert(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::SignedCurve25519,
                    key_id.as_str().into(),
                ),
                signed_key.to_raw(),
            );
        }

        Ok(one_time_key_map)
    }

    /// Generate, sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded returns an empty error.
    pub(crate) async fn signed_one_time_keys(
        &self,
    ) -> Result<BTreeMap<Box<DeviceKeyId>, Raw<ruma::encryption::OneTimeKey>>, ()> {
        let _ = self.generate_one_time_keys().await?;
        self.signed_one_time_keys_helper().await
    }

    /// Create a new session with another account given a one-time key.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `their_one_time_key` - A signed one-time key that the other account
    /// created and shared with us.
    pub(crate) async fn create_outbound_session_helper(
        &self,
        their_identity_key: &str,
        their_one_time_key: &SignedKey,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .lock()
            .await
            .create_outbound_session(their_identity_key, &their_one_time_key.key())?;

        let now = Instant::now();
        let session_id = session.session_id();

        Ok(Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            sender_key: their_identity_key.into(),
            created_using_fallback_key: their_one_time_key.fallback(),
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        })
    }

    /// Create a new session with another account given a one-time key and a
    /// device.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `device` - The other account's device.
    ///
    /// * `key_map` - A map from the algorithm and device id to the one-time key
    ///   that the other account created and shared with us.
    pub(crate) async fn create_outbound_session(
        &self,
        device: ReadOnlyDevice,
        key_map: &BTreeMap<Box<DeviceKeyId>, Raw<ruma::encryption::OneTimeKey>>,
    ) -> Result<Session, SessionCreationError> {
        let one_time_key = key_map.values().next().ok_or_else(|| {
            SessionCreationError::OneTimeKeyMissing(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let one_time_key: SignedKey = match one_time_key.deserialize_as() {
            Ok(OneTimeKey::SignedKey(k)) => k,
            Ok(OneTimeKey::Key(_)) => {
                return Err(SessionCreationError::OneTimeKeyNotSigned(
                    device.user_id().to_owned(),
                    device.device_id().into(),
                ));
            }
            Ok(_) => {
                return Err(SessionCreationError::OneTimeKeyUnknown(
                    device.user_id().to_owned(),
                    device.device_id().into(),
                ));
            }
            Err(e) => return Err(SessionCreationError::InvalidJson(e)),
        };

        device.verify_one_time_key(&one_time_key).map_err(|e| {
            SessionCreationError::InvalidSignature(
                device.user_id().to_owned(),
                device.device_id().into(),
                e,
            )
        })?;

        let curve_key = device.get_key(DeviceKeyAlgorithm::Curve25519).ok_or_else(|| {
            SessionCreationError::DeviceMissingCurveKey(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        self.create_outbound_session_helper(curve_key, &one_time_key).await.map_err(|e| {
            SessionCreationError::OlmError(
                device.user_id().to_owned(),
                device.device_id().into(),
                e,
            )
        })
    }

    /// Create a new session with another account given a pre-key Olm message.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identitiy/curve25519 key.
    ///
    /// * `message` - A pre-key Olm message that was sent to us by the other
    /// account.
    pub(crate) async fn create_inbound_session(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<Session, OlmSessionError> {
        let session =
            self.inner.lock().await.create_inbound_session_from(their_identity_key, message)?;

        self.inner.lock().await.remove_one_time_keys(&session).expect(
            "Session was successfully created but the account doesn't hold a matching one-time key",
        );

        let now = Instant::now();
        let session_id = session.session_id();

        Ok(Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            sender_key: their_identity_key.into(),
            created_using_fallback_key: false,
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        })
    }

    /// Create a group session pair.
    ///
    /// This session pair can be used to encrypt and decrypt messages meant for
    /// a large group of participants.
    ///
    /// The outbound session is used to encrypt messages while the inbound one
    /// is used to decrypt messages encrypted by the outbound one.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room where the group session will be used.
    ///
    /// * `settings` - Settings determining the algorithm and rotation period of
    /// the outbound group session.
    pub(crate) async fn create_group_session_pair(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> Result<(OutboundGroupSession, InboundGroupSession), ()> {
        if settings.algorithm != EventEncryptionAlgorithm::MegolmV1AesSha2 {
            return Err(());
        }

        let visibility = settings.history_visibility.clone();

        let outbound = OutboundGroupSession::new(
            self.device_id.clone(),
            self.identity_keys.clone(),
            room_id,
            settings,
        );
        let identity_keys = self.identity_keys();

        let sender_key = identity_keys.curve25519();
        let signing_key = identity_keys.ed25519();

        let inbound = InboundGroupSession::new(
            sender_key,
            signing_key,
            room_id,
            outbound.session_key().await,
            Some(visibility),
        )
        .expect("Can't create inbound group session from a newly created outbound group session");

        Ok((outbound, inbound))
    }

    #[cfg(test)]
    pub(crate) async fn create_group_session_pair_with_defaults(
        &self,
        room_id: &RoomId,
    ) -> Result<(OutboundGroupSession, InboundGroupSession), ()> {
        self.create_group_session_pair(room_id, EncryptionSettings::default()).await
    }

    #[cfg(test)]
    pub(crate) async fn create_session_for(&self, other: &ReadOnlyAccount) -> (Session, Session) {
        use ruma::events::{dummy::ToDeviceDummyEventContent, AnyToDeviceEventContent};

        other.generate_one_time_keys_helper(1).await;
        let one_time = other.signed_one_time_keys().await.unwrap();

        let device = ReadOnlyDevice::from_account(other).await;

        let mut our_session =
            self.create_outbound_session(device.clone(), &one_time).await.unwrap();

        other.mark_keys_as_published().await;

        let message = our_session
            .encrypt(&device, AnyToDeviceEventContent::Dummy(ToDeviceDummyEventContent::new()))
            .await
            .unwrap();
        let content = if let EncryptedEventScheme::OlmV1Curve25519AesSha2(c) = message.scheme {
            c
        } else {
            panic!("Invalid encrypted event algorithm");
        };

        let own_ciphertext = content.ciphertext.get(other.identity_keys.curve25519()).unwrap();
        let message_type: u8 = own_ciphertext.message_type.try_into().unwrap();

        let message =
            OlmMessage::from_type_and_ciphertext(message_type.into(), own_ciphertext.body.clone())
                .unwrap();
        let prekey = if let OlmMessage::PreKey(m) = message.clone() {
            m
        } else {
            panic!("Wrong Olm message type");
        };

        let our_device = ReadOnlyDevice::from_account(self).await;
        let mut other_session = other
            .create_inbound_session(
                our_device
                    .keys()
                    .get(&DeviceKeyId::from_parts(
                        DeviceKeyAlgorithm::Curve25519,
                        our_device.device_id(),
                    ))
                    .unwrap(),
                prekey,
            )
            .await
            .unwrap();

        other_session.decrypt(message).await.unwrap();

        (our_session, other_session)
    }
}

impl PartialEq for ReadOnlyAccount {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared() == other.shared()
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ops::Deref,
    };

    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, UserId};

    use super::ReadOnlyAccount;
    use crate::error::OlmResult as Result;

    fn user_id() -> &'static UserId {
        user_id!("@alice:localhost")
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    #[async_test]
    async fn one_time_key_creation() -> Result<()> {
        let account = ReadOnlyAccount::new(user_id(), device_id());

        let one_time_keys = account
            .keys_for_upload()
            .await
            .map(|(_, k, _)| k)
            .expect("Initial keys can't be generated");

        let second_one_time_keys = account
            .keys_for_upload()
            .await
            .map(|(_, k, _)| k)
            .expect("Second round of one-time keys isn't generated");

        let device_key_ids: BTreeSet<&DeviceKeyId> =
            one_time_keys.keys().map(Deref::deref).collect();
        let second_device_key_ids: BTreeSet<&DeviceKeyId> =
            second_one_time_keys.keys().map(Deref::deref).collect();

        assert_eq!(device_key_ids, second_device_key_ids);

        account.mark_keys_as_published().await;
        account.update_uploaded_key_count(50);

        let third_one_time_keys = account.keys_for_upload().await.map(|(_, k, _)| k);

        assert!(third_one_time_keys.is_none());

        account.update_uploaded_key_count(0);

        let fourth_one_time_keys = account
            .keys_for_upload()
            .await
            .map(|(_, k, _)| k)
            .expect("Fourth round of one-time keys isn't generated");

        let fourth_device_key_ids: BTreeSet<&DeviceKeyId> =
            fourth_one_time_keys.keys().map(Deref::deref).collect();

        assert_ne!(device_key_ids, fourth_device_key_ids);

        Ok(())
    }

    #[async_test]
    async fn fallback_key_creation() -> Result<()> {
        let account = ReadOnlyAccount::new(&user_id(), &device_id());

        let fallback_keys = account.keys_for_upload().await.map(|(_, _, k)| k);

        // We don't create fallback keys since we don't know if the server
        // supports them, we need to receive a sync response to decide if we're
        // going to create them or not.
        assert!(fallback_keys.is_none());

        let one_time_keys = BTreeMap::from([(DeviceKeyAlgorithm::SignedCurve25519, 50u8.into())]);

        // A `None` here means that the server doesn't support fallback keys, no
        // fallback key gets uploaded.
        account.update_key_counts(&one_time_keys, None).await;
        let fallback_keys = account.keys_for_upload().await.map(|(_, _, k)| k);
        assert!(fallback_keys.is_none());

        // The empty array means that the server supports fallback keys but
        // there isn't a unused fallback key on the server. This time we upload
        // a fallback key.
        let unused_fallback_keys = &[];
        account.update_key_counts(&one_time_keys, Some(unused_fallback_keys.as_ref())).await;
        let fallback_keys = account.keys_for_upload().await.map(|(_, _, k)| k);
        assert!(fallback_keys.is_some());
        account.mark_keys_as_published().await;

        // There's an unused fallback key on the server, nothing to do here.
        let unused_fallback_keys = &[DeviceKeyAlgorithm::SignedCurve25519];
        account.update_key_counts(&one_time_keys, Some(unused_fallback_keys.as_ref())).await;
        let fallback_keys = account.keys_for_upload().await.map(|(_, _, k)| k);
        assert!(fallback_keys.is_none());

        Ok(())
    }
}

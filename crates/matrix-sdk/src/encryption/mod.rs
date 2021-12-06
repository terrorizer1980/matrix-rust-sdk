// Copyright 2021 The Matrix.org Foundation C.I.C.
// Copyright 2021 Damir Jelić
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

#![cfg_attr(rustfmt, rustfmt_skip)]
//! End-to-end encryption related types
//!
//! Matrix has support for end-to-end encrypted messaging, this module contains
//! types related to end-to-end encryption, describes a bit how E2EE works in
//! the matrix-sdk, and how to set your [`Client`] up to support E2EE.
//!
//! Jump to the [Client Setup](#client-setup) section if you don't care how E2EE
//! works under the hood.
//!
//! # End-to-end encryption
//!
//! While all messages in Matrix land are transferred to the server in an
//! encrypted manner, rooms can be marked as end-to-end encrypted. If a room is
//! marked as end-to-end encrypted, using a `m.room.encrypted` state event, all
//! messages that are sent to this room will be encrypted for the individual
//! room members. This means that the server won't be able to read messages that
//! get sent to such a room.
//!
//! ```text
//!                               ┌──────────────┐
//!                               │  Homeserver  │
//!      ┌───────┐                │              │                ┌───────┐
//!      │ Alice │═══════════════►│  unencrypted │═══════════════►│  Bob  │
//!      └───────┘   encrypted    │              │   encrypted    └───────┘
//!                               └──────────────┘
//! ```
//!
//! ```text
//!                               ┌──────────────┐
//!                               │  Homeserver  │
//!      ┌───────┐                │              │                ┌───────┐
//!      │ Alice │≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡►│─────────────►│≡≡≡≡≡≡≡≡≡≡≡≡≡≡≡►│  Bob  │
//!      └───────┘   encrypted    │   encrypted  │   encrypted    └───────┘
//!                               └──────────────┘
//! ```
//!
//! ## Encrypting for each end
//!
//! We already mentioned that a message in a end-to-end encrypted world needs to
//! be encrypted for each individual member, though that isn't completely
//! correct. A message needs to be encrypted for each individual *end*. An *end*
//! in Matrix land is a client that communicates with the homeserver. The spec
//! calls an *end* a Device, while other clients might call an *end* a Session.
//!
//! The matrix-sdk represents an *end* as a [`Device`] object. Each individual
//! message should be encrypted for each individual [`Device`] of each
//! individual room member.
//!
//! Since rooms might grow quite big, encrypting each message for every
//! [`Device`] becomes quickly unsustainable. Because of that room keys have
//! been introduced.
//!
//! ## Room keys
//!
//! Room keys remove the need to encrypt each message for each *end*.
//! Instead a room key needs to be shared with each *end*, after that a message
//! can be encrypted in a single, O(1), step.
//!
//! A room key is backed by a [Megolm] session, which in turn consists two
//! parts. The first part, the outbound group session is used for encryption,
//! this one never leaves your device. The second part is the inbound group
//! session, which is shared with each *end*.
//!
//! ```text
//!             ┌────────────────────────┬───────────────────────┐
//!             │       Encryption       │      Decryption       │
//!             ├────────────────────────┼───────────────────────┤
//!             │ Outbound group session │ Inbound group session │
//!             └────────────────────────┴───────────────────────┘
//! ```
//!
//! ### Lifetime of a room key
//!
//! 1. Create a room key
//! 2. Share the room key with each participant
//! 3. Encrypt messages using the room key
//! 4. If needed, rotate the room key and go back to 1
//!
//! The `m.room.encryption` state event of the room decides how long a room key
//! should be used. By default this is for 100 messages or for 1 week, whichever
//! comes first.
//!
//! ### Decrypting the room history
//!
//! Since room keys get relatively often rotated, each room key will need to be
//! stored, otherwise we won't be able to decrypt historical messages. The SDK
//! stores all room keys locally in a encrypted manner.
//!
//! Besides storing them as part of the SDK store, users can export room keys
//! using the [`Client::export_keys`] method.
//!
//! # Verification
//!
//! One important aspect of end-to-end encryption is to check that the *end* you
//! are communicating with is indeed the person you expect. This checking is
//! done in Matrix via interactive verification. While interactively verifying,
//! we'll need to exchange some critical piece of information over another
//! communication channel, over the phone, or in person are good candidates
//! for such a channel.
//!
//! Usually each *end* will need to verify every *end* it communicates with. An
//! *end* is represented as a [`Device`] in the matrix-sdk. This gets rather
//! complicated quickly as is shown bellow, with Alice and Bob each having two
//! devices. Each arrow represents who needs to verify whom for the
//! communication between Alice and Bob to be considered secure.
//!
//! ```text
//!
//!               ┌───────────────────────────────────────────┐
//!               ▼                                           │
//!         ┌───────────┐                                ┌────┴────┐
//!       ┌►│Alice Phone├───────────────────────────────►│Bob Phone│◄──┐
//!       │ └─────┬─────┘                                └─────┬───┘   │
//!       │       ▼                                            ▼       │
//!       │ ┌────────────┐                               ┌───────────┐ │
//!       └─┤Alice Laptop├──────────────────────────────►│Bob Desktop├─┘
//!         └────────────┘                               └─────┬─────┘
//!               ▲                                            │
//!               └────────────────────────────────────────────┘
//!
//! ```
//!
//! To simplify things and lower the amount of devices a user needs to verify
//! cross signing has been introduced. Cross signing adds a concept of a user
//! identity which is represented in the matrix-sdk using the [`UserIdentity`]
//! struct. This way Alice and Bob only need to verify their own devices and
//! each others user identity for the communication to be considered secure.
//!
//! ```text
//!
//!            ┌─────────────────────────────────────────────────┐
//!            │   ┌─────────────────────────────────────────┐   │
//!            ▼   │                                         ▼   │
//!     ┌──────────┴─────────┐                   ┌───────────────┴──────┐
//!     │┌──────────────────┐│                   │  ┌────────────────┐  │
//!     ││Alice UserIdentity││                   │  │Bob UserIdentity│  │
//!     │└───┬─────────┬────┘│                   │  └─┬───────────┬──┘  │
//!     │    │         │     │                   │    │           │     │
//!     │    ▼         ▼     │                   │    ▼           ▼     │
//!     │┌───────┐ ┌────────┐│                   │┌───────┐  ┌─────────┐│
//!     ││ Alice │ │ Alice  ││                   ││  Bob  │  │   Bob   ││
//!     ││ Phone │ │ Laptop ││                   ││ Phone │  │ Desktop ││
//!     │└───────┘ └────────┘│                   │└───────┘  └─────────┘│
//!     └────────────────────┘                   └──────────────────────┘
//!
//! ```
//!
//! More info about devices and identities can be found in the [`identities`]
//! module.
//!
//! To add interactive verification support to your client please see the
//! [`verification`] module, also check out the documentation for the
//! [`Device::verified()`] method, which explains in more detail what it means
//! for a [`Device`] to be verified.
//!
//! # Client setup
//!
//! The matrix-sdk aims to provide encryption support transparently. If
//! encryption is enabled and correctly set up, events that need to be encrypted
//! will be encrypted automatically. Events will also be decrypted
//! automatically.
//!
//! Please note that, unless a client is specifically set up to ignore
//! unverified devices, verifying devices is **not** necessary for encryption
//! to work.
//!
//! 1. Make sure the `encryption` feature is enabled.
//! 2. Configure a store path with the [`ClientConfig::store_path`] method.
//!
//! ## Restoring a client
//!
//! Restoring a Client is relatively easy, still some things need to be kept in
//! mind before doing so.
//!
//! There are two ways one might wish to restore a [`Client`]:
//! 1. Using an access token
//! 2. Using the password
//!
//! Initially, logging in creates a device ID and access token on the server,
//! those two are directly connected to each other, more on this relationship
//! can be found in the [spec].
//!
//! After we log in the client will upload the end-to-end encryption related
//! [device keys] to the server. Those device keys cannot be replaced once they
//! have been uploaded and tied to a device ID.
//!
//! ### Using an access token
//!
//! 1. Log in with the password using [`Client::login()`] setting the
//!    `device_id` argument to `None`.
//! 2. Store the access token, preferably somewhere secure.
//! 3. Use [`Client::restore_login()`] the next time the client starts.
//!
//! **Note** that the access token is directly connected to a device ID that
//! lives on a server. If you're skipping step one of this method, remember that
//! you **can't** use an access token that already has some device keys tied to
//! the device ID.
//!
//! ### Using a password.
//!
//! 1. Log in using [`Client::login()`] setting the `device_id` argument to `None`.
//! 2. Store the `device_id` that was returned in the login response from the
//! server.
//! 3. Use [`Client::login()`] the next time the client starts, make sure to
//! **set** `device_id` this time to the stored `device_id` from the previous
//! step. This will replace the access token from the previous login call but
//! won't create a new device.
//!
//! **Note** that the default store supports only a single device, logging in
//! with a different device id (either `None` or a device ID of another client)
//! is **not** supported using the default store.
//!
//! ## Common pitfalls
//!
//! | Failure | Cause | Fix |
//! | ------------------- | ----- | ----------- |
//! | No messages get encrypted nor decrypted | The `encryption` feature is disabled | [Enable the feature in your `Cargo.toml` file] |
//! | Messages that were decryptable aren't after a restart | Storage isn't setup to be persistent | Setup storage with [`ClientConfig::store_path`] |
//! | Messages are encrypted but can't be decrypted | The access token that the client is using is tied to another device | Clear storage to create a new device, read the [Restoring a Client] section |
//! | Messages don't get encrypted but get decrypted | The `m.room.encryption` event is missing | Make sure encryption is [enabled] for the room and the event isn't [filtered] out, otherwise it might be a deserialization bug |
//!
//! [Enable the feature in your `Cargo.toml` file]: https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#choosing-features
//! [Megolm]: https://gitlab.matrix.org/matrix-org/olm/blob/master/docs/megolm.md
//! [`ClientConfig::store_path`]: crate::config::ClientConfig::store_path
//! [`UserIdentity`]: #struct.verification.UserIdentity
//! [filtered]: crate::config::SyncSettings::filter
//! [enabled]: crate::room::Joined::enable_encryption
//! [Restoring a Client]: #restoring-a-client
//! [spec]: https://spec.matrix.org/unstable/client-server-api/#relationship-between-access-tokens-and-devices
//! [device keys]: https://spec.matrix.org/unstable/client-server-api/#device-keys

pub mod identities;
pub mod verification;
use std::{
    collections::{BTreeMap, HashSet},
    io::{Read,Write},
    path::PathBuf,
    result::Result as StdResult, iter,
};

use futures_util::stream::{self, StreamExt};
pub use matrix_sdk_base::crypto::{MediaEncryptionInfo, LocalTrust, RoomKeyImportResult};
use matrix_sdk_base::{
    crypto::{
        store::CryptoStoreError, CrossSigningStatus, OutgoingRequest, RoomMessageRequest,
        ToDeviceRequest,
    },
    deserialized_responses::RoomEvent,
};
use matrix_sdk_common::{instant::Duration, uuid::Uuid};
use ruma::{
    api::client::r0::{
        backup::add_backup_keys::Response as KeysBackupResponse,
        keys::{get_keys, upload_keys, upload_signing_keys::Request as UploadSigningKeysRequest},
        message::send_message_event,
        to_device::send_event_to_device::{
            Request as RumaToDeviceRequest, Response as ToDeviceResponse,
        },
        uiaa::AuthData,
    },
    assign,
    events::{AnyMessageEvent, AnyRoomEvent, AnySyncMessageEvent, EventType},
    serde::Raw,
    DeviceId, UserId,
};
use tracing::{debug, instrument, trace, warn};

use crate::{
    encryption::{
        identities::{Device, UserDevices},
        verification::{SasVerification, Verification, VerificationRequest},
    },
    error::{HttpError, HttpResult, RoomKeyImportError},
    room, Client, Error, Result,
};

impl Client {
    /// Get the public ed25519 key of our own device. This is usually what is
    /// called the fingerprint of the device.
    #[cfg(feature = "encryption")]
    pub async fn ed25519_key(&self) -> Option<String> {
        self.olm_machine().await.map(|o| o.identity_keys().ed25519().to_owned())
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    #[cfg(feature = "encryption")]
    pub async fn cross_signing_status(&self) -> Option<CrossSigningStatus> {
        if let Some(machine) = self.olm_machine().await {
            Some(machine.cross_signing_status().await)
        } else {
            None
        }
    }

    /// Get all the tracked users we know about
    ///
    /// Tracked users are users for which we keep the device list of E2EE
    /// capable devices up to date.
    #[cfg(feature = "encryption")]
    pub async fn tracked_users(&self) -> HashSet<Box<UserId>> {
        self.olm_machine().await.map(|o| o.tracked_users()).unwrap_or_default()
    }

    /// Get a verification object with the given flow id.
    #[cfg(feature = "encryption")]
    pub async fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        let olm = self.olm_machine().await?;
        olm.get_verification(user_id, flow_id).map(|v| match v {
            matrix_sdk_base::crypto::Verification::SasV1(s) => {
                SasVerification { inner: s, client: self.clone() }.into()
            }
            #[cfg(feature = "qrcode")]
            matrix_sdk_base::crypto::Verification::QrV1(qr) => {
                verification::QrVerification { inner: qr, client: self.clone() }.into()
            }
        })
    }

    /// Get a `VerificationRequest` object for the given user with the given
    /// flow id.
    #[cfg(feature = "encryption")]
    pub async fn get_verification_request(
        &self,
        user_id: &UserId,
        flow_id: impl AsRef<str>,
    ) -> Option<VerificationRequest> {
        let olm = self.olm_machine().await?;

        olm.get_verification_request(user_id, flow_id)
            .map(|r| VerificationRequest { inner: r, client: self.clone() })
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::{device_id, UserId}};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = Box::<UserId>::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// if let Some(device) = client.get_device(&alice, device_id!("DEVICEID")).await? {
    ///     println!("{:?}", device.verified());
    ///
    ///     if !device.verified() {
    ///         let verification = device.request_verification().await?;
    ///     }
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    #[cfg(feature = "encryption")]
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StdResult<Option<Device>, CryptoStoreError> {
        let device = self.base_client().get_device(user_id, device_id).await?;

        Ok(device.map(|d| Device { inner: d, client: self.clone() }))
    }

    /// Get a map holding all the devices of an user.
    ///
    /// This will always return an empty map if the client hasn't been logged
    /// in.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = Box::<UserId>::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let devices = client.get_user_devices(&alice).await?;
    ///
    /// for device in devices.devices() {
    ///     println!("{:?}", device);
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    #[cfg(feature = "encryption")]
    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StdResult<UserDevices, CryptoStoreError> {
        let devices = self.base_client().get_user_devices(user_id).await?;

        Ok(UserDevices { inner: devices, client: self.clone() })
    }

    /// Get a E2EE identity of an user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the identity belongs to.
    ///
    /// Returns a `UserIdentity` if one is found and the crypto store
    /// didn't throw an error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::{Client, ruma::UserId};
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let alice = Box::<UserId>::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// let user = client.get_user_identity(&alice).await?;
    ///
    /// if let Some(user) = user {
    ///     println!("{:?}", user.verified());
    ///
    ///     let verification = user.request_verification().await?;
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    #[cfg(feature = "encryption")]
    pub async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> StdResult<Option<crate::encryption::identities::UserIdentity>, CryptoStoreError> {
        use crate::encryption::identities::UserIdentity;

        if let Some(olm) = self.olm_machine().await {
            let identity = olm.get_identity(user_id).await?;

            Ok(identity.map(|i| match i {
                matrix_sdk_base::crypto::UserIdentities::Own(i) => {
                    UserIdentity::new_own(self.clone(), i)
                }
                matrix_sdk_base::crypto::UserIdentities::Other(i) => {
                    UserIdentity::new(self.clone(), i, self.get_dm_room(user_id))
                }
            }))
        } else {
            Ok(None)
        }
    }

    /// Create and upload a new cross signing identity.
    ///
    /// # Arguments
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::{convert::TryFrom, collections::BTreeMap};
    /// # use matrix_sdk::{
    /// #     ruma::{api::client::r0::uiaa, assign, UserId},
    /// #     Client,
    /// # };
    /// # use url::Url;
    /// # use futures::executor::block_on;
    /// # use serde_json::json;
    /// # block_on(async {
    /// # let user_id = Box::<UserId>::try_from("@alice:example.org")?;
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver)?;
    /// if let Err(e) = client.bootstrap_cross_signing(None).await {
    ///     if let Some(response) = e.uiaa_response() {
    ///         let auth_data = uiaa::AuthData::Password(assign!(
    ///             uiaa::Password::new(uiaa::UserIdentifier::MatrixId("example"), "wordpass"),
    ///             { session: response.session.as_deref() }
    ///         ));
    ///
    ///         client
    ///             .bootstrap_cross_signing(Some(auth_data))
    ///             .await
    ///             .expect("Couldn't bootstrap cross signing")
    ///     } else {
    ///         panic!("Error durign cross signing bootstrap {:#?}", e);
    ///     }
    /// }
    /// # anyhow::Result::<()>::Ok(()) });
    #[cfg(feature = "encryption")]
    pub async fn bootstrap_cross_signing(&self, auth_data: Option<AuthData<'_>>) -> Result<()> {
        use serde_json::value::to_raw_value;

        let olm = self.olm_machine().await.ok_or(Error::AuthenticationRequired)?;

        let (request, signature_request) = olm.bootstrap_cross_signing(false).await?;

        let to_raw = |k| {
            Raw::from_json(to_raw_value(&k).expect("Can't serialize newly created cross signing keys"))
        };

        let request = assign!(UploadSigningKeysRequest::new(), {
            auth: auth_data,
            master_key: request.master_key.map(to_raw),
            self_signing_key: request.self_signing_key.map(to_raw),
            user_signing_key: request.user_signing_key.map(to_raw),
        });

        self.send(request, None).await?;
        self.send(signature_request, None).await?;

        Ok(())
    }

    /// Export E2EE keys that match the given predicate encrypting them with the
    /// given passphrase.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will be saved.
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the
    ///   exported
    /// room keys.
    ///
    /// * `predicate` - A closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true` the `InboundGroupSessoin` will be included in the export,
    /// if the closure returns `false` it will not be included.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// This method will panic if it can't get enough randomness from the OS to
    /// encrypt the exported keys securely.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver)?;
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// // Export all room keys.
    /// client
    ///     .export_keys(path, "secret-passphrase", |_| true)
    ///     .await?;
    ///
    /// // Export only the room keys for a certain room.
    /// let path = PathBuf::from("/home/example/e2e-room-keys.txt");
    /// let room_id = room_id!("!test:localhost");
    ///
    /// client
    ///     .export_keys(path, "secret-passphrase", |s| s.room_id() == room_id)
    ///     .await?;
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    #[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
    pub async fn export_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
        predicate: impl FnMut(&matrix_sdk_base::crypto::olm::InboundGroupSession) -> bool,
    ) -> Result<()> {
        let olm = self.olm_machine().await.ok_or(Error::AuthenticationRequired)?;

        let keys = olm.export_keys(predicate).await?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_owned());

        let encrypt = move || -> Result<()> {
            let export: String =
                matrix_sdk_base::crypto::encrypt_key_export(&keys, &passphrase, 500_000)?;
            let mut file = std::fs::File::create(path)?;
            file.write_all(&export.into_bytes())?;
            Ok(())
        };

        let task = tokio::task::spawn_blocking(encrypt);
        task.await.expect("Task join error")
    }

    /// Import E2EE keys from the given file path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will can be found.
    ///
    /// * `passphrase` - The passphrase that should be used to decrypt the
    /// exported room keys.
    ///
    /// Returns a tuple of numbers that represent the number of sessions that
    /// were imported and the total number of sessions that were found in the
    /// key export.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver)?;
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// let result = client.import_keys(path, "secret-passphrase").await?;
    ///
    /// println!(
    ///     "Imported {} room keys out of {}",
    ///     result.imported_count, result.total_count
    /// );
    /// # anyhow::Result::<()>::Ok(()) });
    /// ```
    #[cfg(all(feature = "encryption", not(target_arch = "wasm32")))]
    pub async fn import_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
    ) -> StdResult<RoomKeyImportResult, RoomKeyImportError> {
        let olm = self.olm_machine().await.ok_or(RoomKeyImportError::StoreClosed)?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_owned());

        let decrypt = move || {
            let file = std::fs::File::open(path)?;
            matrix_sdk_base::crypto::decrypt_key_export(file, &passphrase)
        };

        let task = tokio::task::spawn_blocking(decrypt);
        let import = task.await.expect("Task join error")?;

        Ok(olm.import_keys(import, false, |_, _| {}).await?)
    }

    /// Tries to decrypt a `AnyRoomEvent`. Returns unencrypted room event when
    /// decryption fails.
    #[cfg(feature = "encryption")]
    pub(crate) async fn decrypt_room_event(
        &self,
        event: &AnyRoomEvent,
    ) -> serde_json::Result<RoomEvent> {
        use ruma::serde::JsonObject;

        if let Some(machine) = self.olm_machine().await {
            if let AnyRoomEvent::Message(event) = event {
                if let AnyMessageEvent::RoomEncrypted(_) = event {
                    let room_id = event.room_id();
                    // Turn the AnyMessageEvent into a AnySyncMessageEvent
                    let event = event.clone().into();

                    if let AnySyncMessageEvent::RoomEncrypted(e) = event {
                        if let Ok(decrypted) = machine.decrypt_room_event(&e, room_id).await {
                            let mut full_event = decrypted.event.deserialize_as::<JsonObject>()?;
                            full_event.insert("room_id".to_owned(), serde_json::to_value(room_id)?);

                            let event =
                                Raw::from_json(serde_json::value::to_raw_value(&full_event)?);
                            let encryption_info = decrypted.encryption_info;

                            // Return decrypted room event
                            return Ok(RoomEvent { event, encryption_info });
                        }
                    }
                }
            }
        }

        // Fallback to still-encrypted room event
        Ok(RoomEvent { event: Raw::new(event)?, encryption_info: None })
    }

    /// Query the server for users device keys.
    ///
    /// # Panics
    ///
    /// Panics if no key query needs to be done.
    #[cfg(feature = "encryption")]
    #[instrument]
    pub(crate) async fn keys_query(
        &self,
        request_id: &Uuid,
        device_keys: BTreeMap<Box<UserId>, Vec<Box<DeviceId>>>,
    ) -> Result<get_keys::Response> {
        let request = assign!(get_keys::Request::new(), { device_keys });

        let response = self.send(request, None).await?;
        self.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    /// Encrypt and upload the file to be read from `reader` and construct an
    /// attachment message with `body` and the specified `content_type`.
    #[cfg(feature = "encryption")]
    pub(crate) async fn prepare_encrypted_attachment_message<R: Read>(
        &self,
        body: &str,
        content_type: &mime::Mime,
        reader: &mut R,
    ) -> Result<ruma::events::room::message::MessageType> {
        let mut reader = matrix_sdk_base::crypto::AttachmentEncryptor::new(reader);

        let response = self.upload(content_type, &mut reader).await?;

        let file: ruma::events::room::EncryptedFile = {
            let keys = reader.finish();
            ruma::events::room::EncryptedFileInit {
                url: response.content_uri,
                key: keys.web_key,
                iv: keys.iv,
                hashes: keys.hashes,
                v: keys.version,
            }
            .into()
        };

        use ruma::events::room::message;
        Ok(match content_type.type_() {
            mime::IMAGE => {
                message::MessageType::Image(message::ImageMessageEventContent::encrypted(body.to_owned(), file))
            }
            mime::AUDIO => {
                message::MessageType::Audio(message::AudioMessageEventContent::encrypted(body.to_owned(), file))
            }
            mime::VIDEO => {
                message::MessageType::Video(message::VideoMessageEventContent::encrypted(body.to_owned(), file))
            }
            _ => message::MessageType::File(message::FileMessageEventContent::encrypted(body.to_owned(), file)),
        })
    }

    #[cfg(feature = "encryption")]
    async fn send_account_data(
        &self,
        content: ruma::events::AnyGlobalAccountDataEventContent,
    ) -> Result<ruma::api::client::r0::config::set_global_account_data::Response> {
        let own_user =
            self.user_id().await.ok_or_else(|| Error::from(HttpError::AuthenticationRequired))?;
        let data = serde_json::value::to_raw_value(&content)?;

        let request = ruma::api::client::r0::config::set_global_account_data::Request::new(
            &data,
            ruma::events::EventContent::event_type(&content),
            &own_user,
        );

        Ok(self.send(request, None).await?)
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn create_dm_room(&self, user_id: Box<UserId>) -> Result<Option<room::Joined>> {
        use ruma::{
            api::client::r0::room::create_room::RoomPreset,
            events::AnyGlobalAccountDataEventContent,
        };

        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        // First we create the DM room, where we invite the user and tell the
        // invitee that the room should be a DM.
        let invite = &[user_id.clone()];

        let request = assign!(
            ruma::api::client::r0::room::create_room::Request::new(),
            {
                invite,
                is_direct: true,
                preset: Some(RoomPreset::TrustedPrivateChat),
            }
        );

        let response = self.send(request, None).await?;

        // Now we need to mark the room as a DM for ourselves, we fetch the
        // existing `m.direct` event and append the room to the list of DMs we
        // have with this user.
        let mut content = self
            .store()
            .get_account_data_event(EventType::Direct)
            .await?
            .map(|e| e.deserialize())
            .transpose()?
            .and_then(|e| {
                if let AnyGlobalAccountDataEventContent::Direct(c) = e.content() {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| ruma::events::direct::DirectEventContent(BTreeMap::new()));

        content.entry(user_id.to_owned()).or_default().push(response.room_id.to_owned());

        // TODO We should probably save the fact that we need to send this out
        // because otherwise we might end up in a state where we have a DM that
        // isn't marked as one.
        self.send_account_data(AnyGlobalAccountDataEventContent::Direct(content)).await?;

        // If the room is already in our store, fetch it, otherwise wait for a
        // sync to be done which should put the room into our store.
        if let Some(room) = self.get_joined_room(&response.room_id) {
            Ok(Some(room))
        } else {
            self.inner.sync_beat.listen().wait_timeout(SYNC_WAIT_TIME);
            Ok(self.get_joined_room(&response.room_id))
        }
    }

    /// Claim one-time keys creating new Olm sessions.
    ///
    /// # Arguments
    ///
    /// * `users` - The list of user/device pairs that we should claim keys for.
    #[cfg(feature = "encryption")]
    #[instrument(skip(users))]
    pub(crate) async fn claim_one_time_keys(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        let _lock = self.inner.key_claim_lock.lock().await;

        if let Some((request_id, request)) = self.base_client().get_missing_sessions(users).await? {
            let response = self.send(request, None).await?;
            self.mark_request_as_sent(&request_id, &response).await?;
        }

        Ok(())
    }

    /// Upload the E2E encryption keys.
    ///
    /// This uploads the long lived device keys as well as the required amount
    /// of one-time keys.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in, or if no encryption keys need to
    /// be uploaded.
    #[cfg(feature = "encryption")]
    #[instrument]
    pub(crate) async fn keys_upload(
        &self,
        request_id: &Uuid,
        request: &upload_keys::Request,
    ) -> Result<upload_keys::Response> {
        debug!(
            device_keys = request.device_keys.is_some(),
            one_time_key_count = request.one_time_keys.len(),
            "Uploading public encryption keys",
        );

        let response = self.send(request.clone(), None).await?;
        self.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn room_send_helper(
        &self,
        request: &RoomMessageRequest,
    ) -> Result<send_message_event::Response> {
        let content = request.content.clone();
        let txn_id = request.txn_id;
        let room_id = &request.room_id;

        self.get_joined_room(room_id)
            .expect("Can't send a message to a room that isn't known to the store")
            .send(content, Some(txn_id))
            .await
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn send_to_device(
        &self,
        request: &ToDeviceRequest,
    ) -> HttpResult<ToDeviceResponse> {
        let txn_id_string = request.txn_id_string();

        let request = RumaToDeviceRequest::new_raw(
            request.event_type.as_str(),
            &txn_id_string,
            request.messages.clone(),
        );

        self.send(request, None).await
    }

    #[cfg(feature = "encryption")]
    pub(crate) async fn send_verification_request(
        &self,
        request: matrix_sdk_base::crypto::OutgoingVerificationRequest,
    ) -> Result<()> {
        match request {
            matrix_sdk_base::crypto::OutgoingVerificationRequest::ToDevice(t) => {
                self.send_to_device(&t).await?;
            }
            matrix_sdk_base::crypto::OutgoingVerificationRequest::InRoom(r) => {
                self.room_send_helper(&r).await?;
            }
        }

        Ok(())
    }

    #[cfg(feature = "encryption")]
    fn get_dm_room(&self, user_id: &UserId) -> Option<room::Joined> {
        let rooms = self.joined_rooms();
        let room_pairs: Vec<_> =
            rooms.iter().map(|r| (r.room_id().to_owned(), r.direct_target())).collect();
        trace!(rooms =? room_pairs, "Finding direct room");

        let room = rooms.into_iter().find(|r| r.direct_target().as_deref() == Some(user_id));

        trace!(room =? room, "Found room");
        room
    }

    async fn send_outgoing_request(&self, r: OutgoingRequest) -> Result<()> {
        use matrix_sdk_base::crypto::OutgoingRequests;

        match r.request() {
            OutgoingRequests::KeysQuery(request) => {
                self.keys_query(r.request_id(), request.device_keys.clone()).await?;
            }
            OutgoingRequests::KeysUpload(request) => {
                self.keys_upload(r.request_id(), request).await?;
            }
            OutgoingRequests::ToDeviceRequest(request) => {
                let response = self.send_to_device(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::SignatureUpload(request) => {
                let response = self.send(request.clone(), None).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::RoomMessage(request) => {
                let response = self.room_send_helper(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::KeysClaim(request) => {
                let response = self.send(request.clone(), None).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::KeysBackup(request) => {
                let response = self.send_backup_request(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
        }

        Ok(())
    }

    async fn send_backup_request(
        &self,
        request: &matrix_sdk_base::crypto::KeysBackupRequest,
    ) -> Result<KeysBackupResponse> {
        let request = ruma::api::client::r0::backup::add_backup_keys::Request::new(
            &request.version,
            request.rooms.to_owned(),
        );

        Ok(self.send(request, None).await?)
    }

    pub(crate) async fn send_outgoing_requests(&self) -> Result<()> {
        const MAX_CONCURRENT_REQUESTS: usize = 20;

        // This is needed because sometimes we need to automatically
        // claim some one-time keys to unwedge an existing Olm session.
        if let Err(e) = self.claim_one_time_keys(iter::empty()).await {
            warn!("Error while claiming one-time keys {:?}", e);
        }

        let outgoing_requests = stream::iter(self.base_client().outgoing_requests().await?)
            .map(|r| self.send_outgoing_request(r));

        let requests = outgoing_requests.buffer_unordered(MAX_CONCURRENT_REQUESTS);

        requests
            .for_each(|r| async move {
                match r {
                    Ok(_) => (),
                    Err(e) => warn!(error =? e, "Error when sending out an outgoing E2EE request"),
                }
            })
            .await;

        Ok(())
    }
}

// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Provides the main OPAQUE API

use crate::{
    errors::{utils::check_slice_size, InternalPakeError, PakeError, ProtocolError},
    group::Group,
    key_exchange::{
        finish_ke, generate_ke1, generate_ke2, generate_ke3, KE1Message, KE1State, KE2Message,
        KE2State, KE3Message, KE1_STATE_LEN, KE2_MESSAGE_LEN,
    },
    keypair::{Key, KeyPair, SizedBytes},
    oprf,
    oprf::OprfClientBytes,
    rkr_encryption::{RKRCipher, RKRCiphertext},
};
use generic_array::{
    typenum::{Unsigned, U32, U64},
    GenericArray,
};
use hkdf::Hkdf;
use rand_core::{CryptoRng, RngCore};
use sha2::{Digest, Sha256};
use std::{convert::TryFrom, marker::PhantomData};
use zeroize::Zeroize;

// Constant string used as salt for HKDF computation
const STR_ENVU: &[u8] = b"EnvU";

/// The length of the "key-derivation key" output by the client registration
/// and login finish steps
pub const DERIVED_KEY_LEN: usize = 32;

// Messages
// =========

/// The message sent by the client to the server, to initiate registration
pub struct RegisterFirstMessage<Grp> {
    /// blinded password information
    alpha: Grp,
}

impl<Grp: Group> TryFrom<&[u8]> for RegisterFirstMessage<Grp> {
    type Error = ProtocolError;
    fn try_from(first_message_bytes: &[u8]) -> Result<Self, Self::Error> {
        // Check that the message is actually containing an element of the
        // correct subgroup
        let arr = GenericArray::from_slice(first_message_bytes);
        let alpha = Grp::from_element_slice(arr)?;
        Ok(Self { alpha })
    }
}

impl<Grp: Group> RegisterFirstMessage<Grp> {
    pub fn to_bytes(&self) -> GenericArray<u8, Grp::ElemLen> {
        self.alpha.to_bytes()
    }
}

/// The answer sent by the server to the user, upon reception of the
/// registration attempt
pub struct RegisterSecondMessage<Grp> {
    /// The server's oprf output
    beta: Grp,
}

impl<Grp> TryFrom<&[u8]> for RegisterSecondMessage<Grp>
where
    Grp: Group,
{
    type Error = ProtocolError;

    fn try_from(second_message_bytes: &[u8]) -> Result<Self, Self::Error> {
        let checked_slice = check_slice_size(
            second_message_bytes,
            Grp::ElemLen::to_usize(),
            "second_message_bytes",
        )?;
        // Check that the message is actually containing an element of the
        // correct subgroup
        let arr = GenericArray::from_slice(&checked_slice);
        let beta = Grp::from_element_slice(arr)?;
        Ok(Self { beta })
    }
}

impl<Grp> RegisterSecondMessage<Grp>
where
    Grp: Group,
{
    pub fn to_bytes(&self) -> Vec<u8> {
        self.beta.to_bytes().to_vec()
    }
}

/// The final message from the client, containing encrypted cryptographic
/// identifiers
pub struct RegisterThirdMessage<Aead, KeyFormat: KeyPair> {
    /// The "envelope" generated by the user, containing encrypted
    /// cryptographic identifiers
    envelope: RKRCiphertext<Aead>,
    /// The user's public key
    client_s_pk: KeyFormat::Repr,
}

impl<Aead, KeyFormat> RegisterThirdMessage<Aead, KeyFormat>
where
    Aead: aead::Aead + aead::NewAead<KeySize = U32>,
    KeyFormat: KeyPair,
{
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut res = Vec::new();
        res.extend(self.envelope.to_bytes());
        res.extend(self.client_s_pk.to_arr());
        res
    }
}

impl<Aead, KeyFormat> TryFrom<&[u8]> for RegisterThirdMessage<Aead, KeyFormat>
where
    Aead: aead::Aead + aead::NewAead<KeySize = U32>,
    KeyFormat: KeyPair,
{
    type Error = ProtocolError;

    fn try_from(third_message_bytes: &[u8]) -> Result<Self, Self::Error> {
        let rkr_size = RKRCiphertext::<Aead>::rkr_with_nonce_size();
        let key_len = <KeyFormat::Repr as SizedBytes>::Len::to_usize();
        let checked_bytes =
            check_slice_size(third_message_bytes, rkr_size + key_len, "third_message")?;
        let unchecked_client_s_pk = KeyFormat::Repr::from_bytes(&checked_bytes[rkr_size..])?;
        let client_s_pk = KeyFormat::check_public_key(unchecked_client_s_pk)?;

        Ok(Self {
            envelope: RKRCiphertext::from_bytes(&checked_bytes[..rkr_size])?,
            client_s_pk,
        })
    }
}

/// The message sent by the user to the server, to initiate registration
pub struct LoginFirstMessage<Grp> {
    /// blinded password information
    alpha: Grp,
    ke1_message: KE1Message,
}

impl<Grp: Group> TryFrom<&[u8]> for LoginFirstMessage<Grp> {
    type Error = ProtocolError;
    fn try_from(first_message_bytes: &[u8]) -> Result<Self, Self::Error> {
        // Check that the message is actually containing an element of the
        // correct subgroup
        let elem_len = Grp::ElemLen::to_usize();
        let arr = GenericArray::from_slice(&first_message_bytes[..elem_len]);
        let alpha = Grp::from_element_slice(arr)?;

        let ke1_message = KE1Message::try_from(&first_message_bytes[elem_len..])?;
        Ok(Self { alpha, ke1_message })
    }
}

impl<Grp: Group> LoginFirstMessage<Grp> {
    pub fn to_bytes(&self) -> Vec<u8> {
        let output: Vec<u8> = [
            self.alpha.to_bytes().as_slice(),
            &self.ke1_message.to_bytes(),
        ]
        .concat();
        output
    }
}

/// The answer sent by the server to the user, upon reception of the
/// login attempt.
pub struct LoginSecondMessage<Aead, Grp> {
    /// the server's oprf output
    beta: Grp,
    /// the user's encrypted information,
    envelope: RKRCiphertext<Aead>,
    ke2_message: KE2Message,
}

impl<Aead, Grp> LoginSecondMessage<Aead, Grp>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
{
    pub fn to_bytes(&self) -> Vec<u8> {
        [
            &self.beta.to_bytes()[..],
            &self.envelope.to_bytes()[..],
            &self.ke2_message.to_bytes()[..],
        ]
        .concat()
    }
}

impl<Aead, Grp> TryFrom<&[u8]> for LoginSecondMessage<Aead, Grp>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
{
    type Error = ProtocolError;
    fn try_from(second_message_bytes: &[u8]) -> Result<Self, Self::Error> {
        let cipher_len = RKRCiphertext::<Aead>::rkr_with_nonce_size();
        let elem_len = Grp::ElemLen::to_usize();
        let checked_slice = check_slice_size(
            second_message_bytes,
            elem_len + cipher_len + KE2_MESSAGE_LEN,
            "login_second_message_bytes",
        )?;

        // Check that the message is actually containing an element of the
        // correct subgroup
        let beta_bytes = &checked_slice[..elem_len];
        let arr = GenericArray::from_slice(beta_bytes);
        let beta = Grp::from_element_slice(arr)?;

        let envelope =
            RKRCiphertext::<Aead>::from_bytes(&checked_slice[elem_len..elem_len + cipher_len])?;
        let ke2_message = KE2Message::try_from(&checked_slice[elem_len + cipher_len..])?;

        Ok(Self {
            beta,
            envelope,
            ke2_message,
        })
    }
}

/// The answer sent by the client to the server, upon reception of the
/// encrypted envelope
pub struct LoginThirdMessage {
    ke3_message: KE3Message,
}

impl TryFrom<&[u8]> for LoginThirdMessage {
    type Error = ProtocolError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let ke3_message = KE3Message::try_from(&bytes[..])?;
        Ok(Self { ke3_message })
    }
}

impl LoginThirdMessage {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.ke3_message.to_bytes()
    }
}

// Registration
// ============

/// The state elements the client holds to register itself
pub struct ClientRegistration<Aead, Grp: Group> {
    /// A choice of symmetric encryption for the envelope
    _aead: PhantomData<Aead>,
    /// a blinding factor
    pub(crate) blinding_factor: Grp::Scalar,
    /// the client's password
    password: Vec<u8>,
}

impl<Aead: aead::NewAead<KeySize = U32> + aead::Aead, Grp: Group> TryFrom<&[u8]>
    for ClientRegistration<Aead, Grp>
{
    type Error = ProtocolError;
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        // Check that the message is actually containing an element of the
        // correct subgroup
        let scalar_len = Grp::ScalarLen::to_usize();
        let blinding_factor_bytes = GenericArray::from_slice(&bytes[..scalar_len]);
        let blinding_factor = Grp::from_scalar_slice(blinding_factor_bytes)?;
        let password = bytes[scalar_len..].to_vec();
        Ok(Self {
            _aead: PhantomData,
            blinding_factor,
            password,
        })
    }
}

impl<Aead, Grp> ClientRegistration<Aead, Grp>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
{
    pub fn to_bytes(&self) -> Vec<u8> {
        let output: Vec<u8> = [
            Grp::scalar_as_bytes(&self.blinding_factor).as_slice(),
            &self.password,
        ]
        .concat();
        output
    }
}

impl<Aead, Grp> ClientRegistration<Aead, Grp>
where
    Grp: Group<ScalarLen = U32, UniformBytesLen = U64>,
{
    /// Returns an initial "blinded" request to send to the server, as well as a ClientRegistration
    ///
    /// # Arguments
    /// * `password` - A user password
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::opaque::ClientRegistration;
    /// # use opaque_ke::errors::ProtocolError;
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// use rand_core::{OsRng, RngCore};
    /// let mut rng = OsRng;
    /// let (register_m1, registration_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut rng)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        password: &[u8],
        pepper: Option<&[u8]>,
        blinding_factor_rng: &mut R,
    ) -> Result<(RegisterFirstMessage<Grp>, Self), ProtocolError> {
        let OprfClientBytes {
            alpha,
            blinding_factor,
        } = oprf::generate_oprf1::<R, Grp>(&password, pepper, blinding_factor_rng)?;

        Ok((
            RegisterFirstMessage::<Grp> { alpha },
            Self {
                _aead: PhantomData,
                blinding_factor,
                password: password.to_vec(),
            },
        ))
    }
}

type ClientRegistrationFinishResult<Aead, KeyFormat> = (
    RegisterThirdMessage<Aead, KeyFormat>,
    GenericArray<u8, <Sha256 as Digest>::OutputSize>,
);

impl<Aead, Grp> ClientRegistration<Aead, Grp>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
{
    /// "Unblinds" the server's answer and returns a final message containing
    /// cryptographic identifiers, to be sent to the server on setup finalization
    ///
    /// # Arguments
    /// * `message` - the server's answer to the initial registration attempt
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{opaque::{ClientRegistration, ServerRegistration}, keypair::{X25519KeyPair, SizedBytes}};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand_core::{OsRng, RngCore};
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = X25519KeyPair::generate_random(&mut server_rng)?;
    /// let (register_m1, client_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut client_rng)?;
    /// let (register_m2, server_state) =
    /// ServerRegistration::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(register_m1, &mut server_rng)?;
    /// let mut client_rng = OsRng;
    /// let register_m3 = client_state.finish::<_, X25519KeyPair>(register_m2, server_kp.public(), &mut client_rng)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish<R: CryptoRng + RngCore, KeyFormat: KeyPair>(
        self,
        r2: RegisterSecondMessage<Grp>,
        server_s_pk: &KeyFormat::Repr,
        rng: &mut R,
    ) -> Result<ClientRegistrationFinishResult<Aead, KeyFormat>, ProtocolError> {
        let client_static_keypair = KeyFormat::generate_random(rng)?;

        let password_derived_key =
            get_password_derived_key::<Grp>(self.password.clone(), r2.beta, &self.blinding_factor)?;
        let h = Hkdf::<Sha256>::new(None, &password_derived_key);
        let mut okm = [0u8; 3 * DERIVED_KEY_LEN];
        h.expand(STR_ENVU, &mut okm)
            .map_err(|_| InternalPakeError::HkdfError)?;
        let encryption_key = &okm[..DERIVED_KEY_LEN];
        let hmac_key = &okm[DERIVED_KEY_LEN..2 * DERIVED_KEY_LEN];
        let kd_key = &okm[2 * DERIVED_KEY_LEN..];

        let envelope = RKRCiphertext::<Aead>::encrypt(
            &encryption_key,
            &hmac_key,
            &client_static_keypair.private().to_arr(),
            &server_s_pk.to_arr(),
            rng,
        )?;

        Ok((
            RegisterThirdMessage {
                envelope,
                client_s_pk: client_static_keypair.public().clone(),
            },
            *GenericArray::from_slice(&kd_key),
        ))
    }
}

// This can't be derived because of the use of a phantom parameter
impl<Aead, Grp: Group> Zeroize for ClientRegistration<Aead, Grp> {
    fn zeroize(&mut self) {
        self.password.zeroize();
        self.blinding_factor.zeroize();
    }
}

impl<Aead, Grp: Group> Drop for ClientRegistration<Aead, Grp> {
    fn drop(&mut self) {
        self.zeroize();
    }
}

// This can't be derived because of the use of a phantom parameter
impl<Aead, Grp: Group, KeyFormat> Zeroize for ClientLogin<Aead, Grp, KeyFormat> {
    fn zeroize(&mut self) {
        self.password.zeroize();
        self.blinding_factor.zeroize();
    }
}

impl<Aead, Grp: Group, KeyFormat> Drop for ClientLogin<Aead, Grp, KeyFormat> {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// The state elements the server holds to record a registration
pub struct ServerRegistration<Aead, Grp: Group, KeyFormat: KeyPair> {
    envelope: Option<RKRCiphertext<Aead>>,
    client_s_pk: Option<KeyFormat::Repr>,
    pub(crate) oprf_key: Grp::Scalar,
}

impl<Aead, Grp, KeyFormat> TryFrom<&[u8]> for ServerRegistration<Aead, Grp, KeyFormat>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
    KeyFormat: KeyPair + PartialEq,
    <KeyFormat::Repr as SizedBytes>::Len: std::ops::Add<<KeyFormat::Repr as SizedBytes>::Len>,
    generic_array::typenum::Sum<
        <KeyFormat::Repr as SizedBytes>::Len,
        <KeyFormat::Repr as SizedBytes>::Len,
    >: generic_array::ArrayLength<u8>,
{
    type Error = ProtocolError;
    fn try_from(server_registration_bytes: &[u8]) -> Result<Self, Self::Error> {
        let key_len = <KeyFormat::Repr as SizedBytes>::Len::to_usize();
        let scalar_len = Grp::ScalarLen::to_usize();
        let rkr_size = RKRCiphertext::<Aead>::rkr_with_nonce_size();

        if server_registration_bytes.len() == scalar_len {
            return Ok(Self {
                oprf_key: Grp::from_scalar_slice(GenericArray::from_slice(
                    server_registration_bytes,
                ))?,
                client_s_pk: None,
                envelope: None,
            });
        }

        let checked_bytes = check_slice_size(
            server_registration_bytes,
            rkr_size + key_len + scalar_len,
            "server_registration_bytes",
        )?;
        let oprf_key_bytes = GenericArray::from_slice(&checked_bytes[..scalar_len]);
        let oprf_key = Grp::from_scalar_slice(oprf_key_bytes)?;
        let unchecked_client_s_pk =
            KeyFormat::Repr::from_bytes(&checked_bytes[scalar_len..scalar_len + key_len])?;
        let client_s_pk = KeyFormat::check_public_key(unchecked_client_s_pk)?;
        Ok(Self {
            envelope: Some(RKRCiphertext::from_bytes(
                &checked_bytes[checked_bytes.len() - rkr_size..],
            )?),
            client_s_pk: Some(client_s_pk),
            oprf_key,
        })
    }
}

impl<Aead, Grp, KeyFormat> ServerRegistration<Aead, Grp, KeyFormat>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
    KeyFormat: KeyPair + PartialEq,
    <KeyFormat::Repr as SizedBytes>::Len: std::ops::Add<<KeyFormat::Repr as SizedBytes>::Len>,
    generic_array::typenum::Sum<
        <KeyFormat::Repr as SizedBytes>::Len,
        <KeyFormat::Repr as SizedBytes>::Len,
    >: generic_array::ArrayLength<u8>,
{
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut output: Vec<u8> = Grp::scalar_as_bytes(&self.oprf_key).to_vec();
        match &self.client_s_pk {
            Some(v) => output.extend_from_slice(&v.to_arr()),
            None => {}
        };
        match &self.envelope {
            Some(v) => output.extend_from_slice(&v.to_bytes()),
            None => {}
        };
        output
    }

    /// From the client's "blinded" password, returns a response to be
    /// sent back to the client, as well as a ServerRegistration
    ///
    /// # Arguments
    /// * `message`   - the initial registration message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{opaque::*, keypair::{X25519KeyPair, SizedBytes}};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand_core::{OsRng, RngCore};
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let (register_m1, client_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut client_rng)?;
    /// let (register_m2, server_state) =
    /// ServerRegistration::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(register_m1, &mut server_rng)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        message: RegisterFirstMessage<Grp>,
        rng: &mut R,
    ) -> Result<(RegisterSecondMessage<Grp>, Self), ProtocolError> {
        // RFC: generate oprf_key (salt) and v_u = g^oprf_key
        let oprf_key = Grp::random_scalar(rng);

        // Compute beta = alpha^oprf_key
        let beta = oprf::generate_oprf2::<Grp>(message.alpha, &oprf_key)?;

        Ok((
            RegisterSecondMessage { beta },
            Self {
                envelope: None,
                client_s_pk: None,
                oprf_key,
            },
        ))
    }

    /// From the client's cryptographic identifiers, fully populates and
    /// returns a ServerRegistration
    ///
    /// # Arguments
    /// * `message` - the final client message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::{opaque::*, keypair::{X25519KeyPair, SizedBytes}};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::KeyPair;
    /// use rand_core::{OsRng, RngCore};
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = X25519KeyPair::generate_random(&mut server_rng)?;
    /// let (register_m1, client_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut client_rng)?;
    /// let (register_m2, server_state) =
    /// ServerRegistration::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(register_m1, &mut server_rng)?;
    /// let mut client_rng = OsRng;
    /// let (register_m3, _opaque_key) = client_state.finish(register_m2, server_kp.public(), &mut client_rng)?;
    /// let client_record = server_state.finish(register_m3)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish(
        self,
        message: RegisterThirdMessage<Aead, KeyFormat>,
    ) -> Result<Self, ProtocolError> {
        Ok(Self {
            envelope: Some(message.envelope),
            client_s_pk: Some(message.client_s_pk),
            oprf_key: self.oprf_key,
        })
    }
}

// Login
// =====

/// The state elements the client holds to perform a login
pub struct ClientLogin<Aead, Grp: Group, KeyFormat> {
    /// A choice of symmetric encryption for the envelope
    _aead: PhantomData<Aead>,
    /// A choice of the keypair type
    _key_format: PhantomData<KeyFormat>,
    /// A blinding factor, which is used to mask (and unmask) secret
    /// information before transmission
    blinding_factor: Grp::Scalar,
    /// The user's password
    password: Vec<u8>,
    ke1_state: KE1State,
}

impl<Aead: aead::NewAead<KeySize = U32> + aead::Aead, Grp: Group, KeyFormat: KeyPair> TryFrom<&[u8]>
    for ClientLogin<Aead, Grp, KeyFormat>
{
    type Error = ProtocolError;
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let scalar_len = Grp::ScalarLen::to_usize();
        let blinding_factor_bytes = GenericArray::from_slice(&bytes[..scalar_len]);
        let blinding_factor = Grp::from_scalar_slice(blinding_factor_bytes)?;
        let ke1_state = KE1State::try_from(&bytes[scalar_len..scalar_len + KE1_STATE_LEN])?;
        let password = bytes[scalar_len + KE1_STATE_LEN..].to_vec();
        Ok(Self {
            _aead: PhantomData,
            _key_format: PhantomData,
            blinding_factor,
            password,
            ke1_state,
        })
    }
}

impl<Aead, Grp, KeyFormat> ClientLogin<Aead, Grp, KeyFormat>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group,
    KeyFormat: KeyPair,
{
    pub fn to_bytes(&self) -> Vec<u8> {
        let output: Vec<u8> = [
            Grp::scalar_as_bytes(&self.blinding_factor).as_slice(),
            &self.ke1_state.to_bytes(),
            &self.password,
        ]
        .concat();
        output
    }
}

type ClientLoginFinishResult = (
    LoginThirdMessage,
    Vec<u8>,
    GenericArray<u8, <Sha256 as Digest>::OutputSize>,
);

impl<Aead, Grp, KeyFormat> ClientLogin<Aead, Grp, KeyFormat>
where
    Aead: aead::NewAead<KeySize = U32> + aead::Aead,
    Grp: Group<UniformBytesLen = U64>,
    KeyFormat: KeyPair<Repr = Key>,
{
    /// Returns an initial "blinded" password request to send to the server, as well as a ClientLogin
    ///
    /// # Arguments
    /// * `password` - A user password
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::opaque::ClientLogin;
    /// # use opaque_ke::errors::ProtocolError;
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// use opaque_ke::keypair::X25519KeyPair;
    /// use rand_core::{OsRng, RngCore};
    /// let mut client_rng = OsRng;
    /// let (login_m1, client_login_state) = ClientLogin::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(b"hunter2", None, &mut client_rng)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<R: RngCore + CryptoRng>(
        password: &[u8],
        pepper: Option<&[u8]>,
        rng: &mut R,
    ) -> Result<(LoginFirstMessage<Grp>, Self), ProtocolError> {
        let OprfClientBytes {
            alpha,
            blinding_factor,
        } = oprf::generate_oprf1::<R, Grp>(&password, pepper, rng)?;

        let (ke1_state, ke1_message) =
            generate_ke1::<_, KeyFormat>(alpha.to_bytes().to_vec(), rng)?;

        let l1 = LoginFirstMessage { alpha, ke1_message };

        Ok((
            l1,
            Self {
                _aead: PhantomData,
                _key_format: PhantomData,
                blinding_factor,
                password: password.to_vec(),
                ke1_state,
            },
        ))
    }

    /// "Unblinds" the server's answer and returns the decrypted assets from
    /// the server
    ///
    /// # Arguments
    /// * `message` - the server's answer to the initial login attempt
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::opaque::{ClientLogin, ServerLogin};
    /// # use opaque_ke::opaque::{ClientRegistration, ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::{X25519KeyPair, KeyPair};
    /// use rand_core::{OsRng, RngCore};
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// let mut client_rng = OsRng;
    /// # let mut server_rng = OsRng;
    /// # let (register_m1, client_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut client_rng)?;
    /// # let server_kp = X25519KeyPair::generate_random(&mut server_rng)?;
    /// # let (register_m2, server_state) = ServerRegistration::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(register_m1, &mut server_rng)?;
    /// # let (register_m3, _opaque_key) = client_state.finish(register_m2, server_kp.public(), &mut client_rng)?;
    /// # let p_file = server_state.finish(register_m3)?;
    /// let (login_m1, client_login_state) = ClientLogin::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(b"hunter2", None, &mut client_rng)?;
    /// let (login_m2, server_login_state) = ServerLogin::start(p_file, &server_kp.private(), login_m1, &mut server_rng)?;
    /// let (login_m3, client_transport, _opaque_key) = client_login_state.finish(login_m2, &server_kp.public(), &mut client_rng)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish<R: RngCore + CryptoRng>(
        self,
        l2: LoginSecondMessage<Aead, Grp>,
        server_s_pk: &KeyFormat::Repr,
        _client_e_sk_rng: &mut R,
    ) -> Result<ClientLoginFinishResult, ProtocolError> {
        let l2_bytes: Vec<u8> = [l2.beta.to_bytes().as_slice(), &l2.envelope.to_bytes()].concat();

        let password_derived_key =
            get_password_derived_key::<Grp>(self.password.clone(), l2.beta, &self.blinding_factor)?;
        let h = Hkdf::<Sha256>::new(None, &password_derived_key);
        let mut okm = [0u8; 3 * DERIVED_KEY_LEN];
        h.expand(STR_ENVU, &mut okm)
            .map_err(|_| InternalPakeError::HkdfError)?;
        let encryption_key = &okm[..DERIVED_KEY_LEN];
        let hmac_key = &okm[DERIVED_KEY_LEN..2 * DERIVED_KEY_LEN];
        let kd_key = &okm[2 * DERIVED_KEY_LEN..];

        let client_s_sk = Key::from_bytes(
            &l2.envelope
                .decrypt(&encryption_key, &hmac_key, &server_s_pk.to_arr())
                .map_err(|e| match e {
                    PakeError::DecryptionHmacError => PakeError::InvalidLoginError,
                    err => err,
                })?,
        )?;

        let (ke3_state, ke3_message) = generate_ke3::<KeyFormat>(
            l2_bytes,
            l2.ke2_message,
            &self.ke1_state,
            server_s_pk.clone(),
            client_s_sk,
        )?;

        Ok((
            LoginThirdMessage { ke3_message },
            ke3_state.shared_secret,
            *GenericArray::from_slice(&kd_key),
        ))
    }
}

/// The state elements the server holds to record a login
pub struct ServerLogin {
    ke2_state: KE2State,
}

impl TryFrom<&[u8]> for ServerLogin {
    type Error = ProtocolError;
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Ok(Self {
            ke2_state: KE2State::try_from(&bytes[..])?,
        })
    }
}

impl ServerLogin {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.ke2_state.to_bytes()
    }

    /// From the client's "blinded"" password, returns a challenge to be
    /// sent back to the client, as well as a ServerLogin
    ///
    /// # Arguments
    /// * `message`   - the initial registration message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::opaque::{ClientLogin, ServerLogin};
    /// # use opaque_ke::opaque::{ClientRegistration,  ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::{KeyPair, X25519KeyPair};
    /// use rand_core::{OsRng, RngCore};
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = X25519KeyPair::generate_random(&mut server_rng)?;
    /// # let (register_m1, client_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut client_rng)?;
    /// # let (register_m2, server_state) =
    /// ServerRegistration::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(register_m1, &mut server_rng)?;
    /// # let (register_m3, _opaque_key) = client_state.finish(register_m2, server_kp.public(), &mut client_rng)?;
    /// # let p_file = server_state.finish(register_m3)?;
    /// let (login_m1, client_login_state) = ClientLogin::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(b"hunter2", None, &mut client_rng)?;
    /// let (login_m2, server_login_state) = ServerLogin::start(p_file, &server_kp.private(), login_m1, &mut server_rng)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn start<
        R: RngCore + CryptoRng,
        Aead: aead::NewAead<KeySize = U32> + aead::Aead,
        Grp: Group,
        KeyFormat: KeyPair<Repr = Key>,
    >(
        password_file: ServerRegistration<Aead, Grp, KeyFormat>,
        server_s_sk: &Key,
        l1: LoginFirstMessage<Grp>,
        rng: &mut R,
    ) -> Result<(LoginSecondMessage<Aead, Grp>, Self), ProtocolError> {
        let l1_bytes = &l1.to_bytes();
        let beta = oprf::generate_oprf2(l1.alpha, &password_file.oprf_key)?;

        let client_s_pk = password_file
            .client_s_pk
            .ok_or(PakeError::EncryptionError)?;
        let envelope = password_file.envelope.ok_or(PakeError::EncryptionError)?;

        let l2_component: Vec<u8> = [beta.to_bytes().as_slice(), &envelope.to_bytes()].concat();

        let (ke2_state, ke2_message) = generate_ke2::<_, KeyFormat>(
            rng,
            l1_bytes.to_vec(),
            l2_component,
            l1.ke1_message.client_e_pk,
            client_s_pk,
            server_s_sk.clone(),
            l1.ke1_message.client_nonce.to_vec(),
        )?;

        let l2 = LoginSecondMessage {
            beta,
            envelope,
            ke2_message,
        };

        Ok((l2, Self { ke2_state }))
    }

    /// From the client's second & final message, check the client's
    /// authentication & produce a message transport
    ///
    /// # Arguments
    /// * `message` - the client's second login message
    ///
    /// # Example
    ///
    /// ```
    /// use opaque_ke::opaque::{ClientLogin, ServerLogin};
    /// # use opaque_ke::opaque::{ClientRegistration,  ServerRegistration};
    /// # use opaque_ke::errors::ProtocolError;
    /// # use opaque_ke::keypair::{KeyPair, X25519KeyPair};
    /// use rand_core::{OsRng, RngCore};
    /// use chacha20poly1305::ChaCha20Poly1305;
    /// use curve25519_dalek::ristretto::RistrettoPoint;
    /// let mut client_rng = OsRng;
    /// let mut server_rng = OsRng;
    /// let server_kp = X25519KeyPair::generate_random(&mut server_rng)?;
    /// # let (register_m1, client_state) = ClientRegistration::<ChaCha20Poly1305, RistrettoPoint>::start(b"hunter2", None, &mut client_rng)?;
    /// # let (register_m2, server_state) =
    /// ServerRegistration::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(register_m1, &mut server_rng)?;
    /// # let (register_m3, _opaque_key) = client_state.finish(register_m2, server_kp.public(), &mut client_rng)?;
    /// # let p_file = server_state.finish(register_m3)?;
    /// let (login_m1, client_login_state) = ClientLogin::<ChaCha20Poly1305, RistrettoPoint, X25519KeyPair>::start(b"hunter2", None, &mut client_rng)?;
    /// let (login_m2, server_login_state) = ServerLogin::start(p_file, &server_kp.private(), login_m1, &mut server_rng)?;
    /// let (login_m3, client_transport, _opaque_key) = client_login_state.finish(login_m2, &server_kp.public(), &mut client_rng)?;
    /// let mut server_transport = server_login_state.finish(login_m3)?;
    /// # Ok::<(), ProtocolError>(())
    /// ```
    pub fn finish(&self, message: LoginThirdMessage) -> Result<Vec<u8>, ProtocolError> {
        finish_ke(message.ke3_message, &self.ke2_state).map_err(|e| match e {
            ProtocolError::VerificationError(PakeError::KeyExchangeMacValidationError) => {
                ProtocolError::VerificationError(PakeError::InvalidLoginError)
            }
            err => err,
        })
    }
}

// Helper functions

fn get_password_derived_key<G: Group>(
    password: Vec<u8>,
    beta: G,
    blinding_factor: &G::Scalar,
) -> Result<GenericArray<u8, <Sha256 as Digest>::OutputSize>, PakeError> {
    Ok(oprf::generate_oprf3::<G>(&password, beta, blinding_factor)?)
}

//! Legacy AirPlay "pair-verify" (curve25519 key agreement + ed25519 signature).
//! Transient pairing: nothing is stored about the client, which keeps the
//! receiver open to any device on the network.

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha512};
use x25519_dalek::{EphemeralSecret, PublicKey};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

#[derive(Default)]
pub struct Pairing {
    /// Shared curve25519 secret once pair-verify step 1 has happened. It is
    /// mixed into the stream key later on.
    pub shared_secret: Option<[u8; 32]>,
    /// Diagnostics: state needed to check the client's step-2 signature.
    pending: Option<(Aes128Ctr, [u8; 32], [u8; 32], [u8; 32])>, // cipher, client curve, client ed, our pub
}

fn derive(salt: &str, secret: &[u8; 32]) -> [u8; 16] {
    let mut h = Sha512::new();
    h.update(salt.as_bytes());
    h.update(secret);
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize()[..16]);
    out
}

impl Pairing {
    /// Step 1: `body` = 4-byte header + client curve key (32) + client ed key (32).
    /// Returns our curve key (32) + encrypted signature (64).
    pub fn verify_step1(&mut self, body: &[u8], identity: &SigningKey) -> Option<Vec<u8>> {
        if body.len() != 68 {
            return None;
        }
        let mut client_curve = [0u8; 32];
        client_curve.copy_from_slice(&body[4..36]);

        let ours = EphemeralSecret::random_from_rng(OsRng);
        let our_pub = PublicKey::from(&ours);
        let shared = ours.diffie_hellman(&PublicKey::from(client_curve)).to_bytes();

        let mut msg = [0u8; 64];
        msg[..32].copy_from_slice(our_pub.as_bytes());
        msg[32..].copy_from_slice(&client_curve);
        let mut sig = identity.sign(&msg).to_bytes();

        let mut cipher = Aes128Ctr::new(
            &derive("Pair-Verify-AES-Key", &shared).into(),
            &derive("Pair-Verify-AES-IV", &shared).into(),
        );
        cipher.apply_keystream(&mut sig);
        self.shared_secret = Some(shared);
        let mut client_ed = [0u8; 32];
        client_ed.copy_from_slice(&body[36..68]);
        self.pending = Some((cipher, client_curve, client_ed, *our_pub.as_bytes()));

        let mut out = our_pub.as_bytes().to_vec();
        out.extend_from_slice(&sig);
        Some(out)
    }

    /// Step 2: check the client's signature (also proves both sides derived the same secret).
    pub fn verify_step2(&mut self, body: &[u8]) -> Option<bool> {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let (mut cipher, client_curve, client_ed, our_pub) = self.pending.take()?;
        if body.len() != 68 {
            return Some(false);
        }
        let mut sig = [0u8; 64];
        sig.copy_from_slice(&body[4..68]);
        cipher.apply_keystream(&mut sig);
        let mut msg = [0u8; 64];
        msg[..32].copy_from_slice(&client_curve);
        msg[32..].copy_from_slice(&our_pub);
        let ok = VerifyingKey::from_bytes(&client_ed)
            .ok()
            .is_some_and(|vk| vk.verify(&msg, &Signature::from_bytes(&sig)).is_ok());
        Some(ok)
    }
}

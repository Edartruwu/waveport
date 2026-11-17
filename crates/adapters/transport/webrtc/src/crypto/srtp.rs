//! The ciphers audio is sealed with: the three profiles of SRTP a
//! browser may ask for.

use aes::cipher::consts::U16;
use aes::cipher::{
    Array, BlockCipherEncrypt, BlockSizeUser, KeyInit, KeyIvInit as _, StreamCipher as _,
};
use aes::{Aes128, Aes256};
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce};
use str0m_proto::crypto::{
    AeadAes128GcmCipher, AeadAes256GcmCipher, Aes128CmSha1_80Cipher, CryptoError, SrtpProvider,
    SupportedAeadAes128Gcm, SupportedAeadAes256Gcm, SupportedAes128CmSha1_80,
};

/// AES-128 in counter mode, as SRTP counts.
type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// What an RTP header is at least, which is the least a packet says
/// besides what is sealed.
const HEADER: usize = 12;

/// The check an AES-GCM packet ends with.
const TAG: usize = 16;

fn refused(why: &str) -> CryptoError {
    CryptoError::Other(why.to_owned())
}

/// Writes `bytes` to the beginning of `output`.
fn put(output: &mut [u8], bytes: &[u8]) -> Result<(), CryptoError> {
    output
        .get_mut(..bytes.len())
        .ok_or_else(|| refused("there is no room for what a cipher wrote"))?
        .copy_from_slice(bytes);
    Ok(())
}

/// AES-128 in counter mode, with a check of HMAC-SHA1 that `str0m` makes
/// itself.
struct CounterMode {
    key: [u8; 16],
}

impl core::fmt::Debug for CounterMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CounterMode").finish_non_exhaustive()
    }
}

impl Aes128CmSha1_80Cipher for CounterMode {
    fn encrypt(
        &mut self,
        iv: &[u8; 16],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        put(output, input)?;
        let written = output.get_mut(..input.len()).unwrap_or_default();
        Aes128Ctr::new(&self.key.into(), iv.into()).apply_keystream(written);
        Ok(())
    }

    fn decrypt(
        &mut self,
        iv: &[u8; 16],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        // Counter mode undoes itself.
        self.encrypt(iv, input, output)
    }
}

/// AES in Galois/counter mode, which seals and checks at once.
struct Gcm<C> {
    cipher: C,
}

impl<C> core::fmt::Debug for Gcm<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gcm").finish_non_exhaustive()
    }
}

impl<C: Aead> Gcm<C> {
    fn seal(
        &self,
        nonce: &Nonce<C::NonceSize>,
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        if aad.len() < HEADER {
            return Err(refused("a packet that is sealed has a header"));
        }
        let payload = Payload { msg: input, aad };
        let sealed = self
            .cipher
            .encrypt(nonce, payload)
            .map_err(|_| refused("AES-GCM could not seal a packet"))?;
        put(output, &sealed)
    }

    fn open(
        &self,
        nonce: &Nonce<C::NonceSize>,
        aads: &[&[u8]],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        if input.len() < TAG {
            return Err(refused("a sealed packet ends with its check"));
        }
        let joined;
        let aad = match aads {
            [one] => *one,
            several => {
                joined = several.concat();
                &joined
            }
        };
        let payload = Payload { msg: input, aad };
        // A packet that was changed on the way, or is nobody's, is
        // refused here.
        let opened = self
            .cipher
            .decrypt(nonce, payload)
            .map_err(|_| refused("AES-GCM could not open a packet"))?;
        put(output, &opened)?;
        Ok(opened.len())
    }
}

impl AeadAes128GcmCipher for Gcm<Aes128Gcm> {
    fn encrypt(
        &mut self,
        iv: &[u8; 12],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        self.seal(&Nonce::from(*iv), aad, input, output)
    }

    fn decrypt(
        &mut self,
        iv: &[u8; 12],
        aads: &[&[u8]],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        self.open(&Nonce::from(*iv), aads, input, output)
    }
}

impl AeadAes256GcmCipher for Gcm<Aes256Gcm> {
    fn encrypt(
        &mut self,
        iv: &[u8; 12],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(), CryptoError> {
        self.seal(&Nonce::from(*iv), aad, input, output)
    }

    fn decrypt(
        &mut self,
        iv: &[u8; 12],
        aads: &[&[u8]],
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        self.open(&Nonce::from(*iv), aads, input, output)
    }
}

#[derive(Debug)]
struct WithCounterMode;

impl SupportedAes128CmSha1_80 for WithCounterMode {
    fn create_cipher(&self, key: [u8; 16], _encrypt: bool) -> Box<dyn Aes128CmSha1_80Cipher> {
        Box::new(CounterMode { key })
    }
}

#[derive(Debug)]
struct WithGcm128;

impl SupportedAeadAes128Gcm for WithGcm128 {
    fn create_cipher(&self, key: [u8; 16], _encrypt: bool) -> Box<dyn AeadAes128GcmCipher> {
        let cipher = Aes128Gcm::new(&key.into());
        Box::new(Gcm { cipher })
    }
}

#[derive(Debug)]
struct WithGcm256;

impl SupportedAeadAes256Gcm for WithGcm256 {
    fn create_cipher(&self, key: [u8; 32], _encrypt: bool) -> Box<dyn AeadAes256GcmCipher> {
        let cipher = Aes256Gcm::new(&key.into());
        Box::new(Gcm { cipher })
    }
}

/// The two blocks the keys of an AES-GCM session are derived with: the
/// block `input`, and a block of `0x10`s, each put through the cipher.
///
/// `str0m` gives a key of the cipher's length, one block, and room for
/// two. It takes what is written for the keys of the session whatever
/// it was given, so given anything else what is written is chance: keys
/// that nobody has, with which nothing of the call can be read, and
/// not the noughts the room held, which anybody has.
fn two_blocks<C>(key: &[u8], input: &[u8], output: &mut [u8])
where
    C: BlockCipherEncrypt + KeyInit + BlockSizeUser<BlockSize = U16>,
{
    let (Ok(cipher), Ok(mut first)) = (C::new_from_slice(key), Array::<u8, U16>::try_from(input))
    else {
        unknowable(output);
        return;
    };
    let Some((one, other)) = output.get_mut(..32).map(|room| room.split_at_mut(16)) else {
        unknowable(output);
        return;
    };
    cipher.encrypt_block(&mut first);
    one.copy_from_slice(&first);
    let mut second = Array::<u8, U16>::from([0x10; 16]);
    cipher.encrypt_block(&mut second);
    other.copy_from_slice(&second);
}

/// Fills `output` with what nobody knows. Where the system has none of
/// that to give, with what is at least not nought.
pub(super) fn unknowable(output: &mut [u8]) {
    if getrandom::fill(output).is_err() {
        output.fill(0xa5);
    }
}

#[derive(Debug)]
pub(super) struct Srtp;

impl SrtpProvider for Srtp {
    fn aes_128_cm_sha1_80(&self) -> &'static dyn SupportedAes128CmSha1_80 {
        &WithCounterMode
    }

    fn aead_aes_128_gcm(&self) -> &'static dyn SupportedAeadAes128Gcm {
        &WithGcm128
    }

    fn aead_aes_256_gcm(&self) -> &'static dyn SupportedAeadAes256Gcm {
        &WithGcm256
    }

    fn srtp_aes_128_ecb_round(&self, key: &[u8], input: &[u8], output: &mut [u8]) {
        two_blocks::<Aes128>(key, input, output);
    }

    fn srtp_aes_256_ecb_round(&self, key: &[u8], input: &[u8], output: &mut [u8]) {
        two_blocks::<Aes256>(key, input, output);
    }
}

#[cfg(test)]
mod tests {
    use aes::Aes128;

    use super::two_blocks;

    #[test]
    fn keys_that_are_derived_from_what_cannot_be_given_are_nobodys() {
        // As it is given: a key of the cipher's length, one block, and
        // room for two. The same goes in, and the same comes out.
        let derived = |key: &[u8], input: &[u8], room: usize| {
            let mut output = vec![0_u8; room];
            two_blocks::<Aes128>(key, input, &mut output);
            output
        };
        let keys = derived(&[7; 16], &[9; 16], 32);
        assert_eq!(keys, derived(&[7; 16], &[9; 16], 32));
        assert_ne!(keys, [0; 32]);
        // A key of another length, a block that is none, room for less
        // than two: what is written is chance, and never the noughts
        // the room held, which would be keys that anybody has.
        let wrong: [(&[u8], &[u8], usize); 3] = [
            (&[7; 15], &[9; 16], 32),
            (&[7; 16], &[9; 15], 32),
            (&[7; 16], &[9; 16], 31),
        ];
        for (key, input, room) in wrong {
            let (one, other) = (derived(key, input, room), derived(key, input, room));
            assert_ne!(one, vec![0; room]);
            assert_ne!(one, other);
        }
    }
}

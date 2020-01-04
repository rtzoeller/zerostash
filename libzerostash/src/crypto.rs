use crate::chunks::ChunkPointer;
use crate::objects::{Object, ObjectId, WriteObject};

use std::convert::TryInto;
use std::sync::Arc;

use blake2::{Blake2s, Digest};
use failure::Fail;
use libc::c_char;
use libsodium_sys::{
    crypto_kdf_KEYBYTES, crypto_kdf_derive_from_key, crypto_pwhash, crypto_pwhash_PASSWD_MIN,
    crypto_pwhash_SALTBYTES, crypto_pwhash_alg_default, crypto_pwhash_memlimit_interactive,
    crypto_pwhash_opslimit_interactive,
};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use zeroize::Zeroizing;

pub const CRYPTO_DIGEST_SIZE: usize = 32;
pub type DigestFn = Blake2s;
pub type CryptoDigest = [u8; CRYPTO_DIGEST_SIZE];
pub type Tag = [u8; 16];

pub trait AeadProvider: Clone + Send {
    fn algo() -> &'static aead::Algorithm;
    fn key(&self) -> &[u8];

    fn encrypt_in_place(key: &[u8], nonce: [u8; 12], buffer: &mut [u8]) -> Tag;
    fn decrypt_in_place(key: &[u8], nonce: [u8; 12], tag: Tag, buffer: &mut [u8]);
}

pub trait Random {
    fn fill(&self, buf: &mut [u8]);
}

pub trait CryptoProvider: Random + Clone + Send {
    fn tag_len(&self) -> usize;

    fn encrypt_chunk(&self, object_id: &WriteObject, hash: &CryptoDigest, data: &mut [u8]) -> Tag;
    fn encrypt_object(&self, object: &mut WriteObject);

    fn decrypt_chunk<T: AsRef<[u8]>>(
        &self,
        target: &mut [u8],
        o: &Object<T>,
        chunk: &ChunkPointer,
    ) -> usize;
    fn decrypt_object<T: AsRef<[u8]>>(&self, object: &Object<T>) -> Vec<u8>;

    fn decrypt_object_into<T: AsRef<[u8]>>(&self, output: &mut [u8], object: &Object<T>);
}

#[derive(Debug, Fail)]
#[fail(display = "Key error")]
pub struct KeyError;

pub struct StashKey {
    master_key: Zeroizing<Vec<u8>>,
    random: Arc<SystemRandom>,
}

impl StashKey {
    pub fn open_stash(
        username: impl AsRef<str>,
        password: impl AsRef<str>,
    ) -> Result<StashKey, KeyError> {
        let saltsize = crypto_pwhash_SALTBYTES.try_into().unwrap();

        let mut hasher = DigestFn::new();
        hasher.input(username.as_ref());
        let salt = hasher.result();

        derive_argon2(&salt[..saltsize], password.as_ref().as_bytes()).map(|k| StashKey {
            master_key: Zeroizing::new(k),
            random: Arc::new(SystemRandom::new()),
        })
    }

    pub fn root_object_id(&self) -> Result<ObjectId, KeyError> {
        derive_subkey(&self.master_key, 0, b"_0s_root").map(ObjectId::from_bytes)
    }

    pub fn get_meta_crypto(&self) -> Result<impl CryptoProvider, KeyError> {
        derive_subkey(&self.master_key, 0, b"_0s_meta")
            .map(|k| ChaCha20Poly1305::new(k, self.random.clone()))
    }

    pub fn get_object_crypto(&self) -> Result<impl CryptoProvider, KeyError> {
        derive_subkey(&self.master_key, 0, b"_0s_obj_")
            .map(|k| ChaCha20Poly1305::new(k, self.random.clone()))
    }
}

#[derive(Clone)]
pub(crate) struct ChaCha20Poly1305 {
    key_bytes: Zeroizing<Vec<u8>>,
    random: Arc<SystemRandom>,
}

impl ChaCha20Poly1305 {
    pub(crate) fn new(key: Vec<u8>, random: Arc<SystemRandom>) -> Self {
        assert_eq!(key.len(), aead::CHACHA20_POLY1305.key_len());

        ChaCha20Poly1305 {
            key_bytes: Zeroizing::new(key),
            random,
        }
    }
}

impl Random for ChaCha20Poly1305 {
    fn fill(&self, buf: &mut [u8]) {
        self.random.fill(buf).unwrap()
    }
}

impl AeadProvider for ChaCha20Poly1305 {
    #[inline]
    fn algo() -> &'static aead::Algorithm {
        &aead::CHACHA20_POLY1305
    }

    #[inline]
    fn key(&self) -> &[u8] {
        &self.key_bytes
    }

    fn encrypt_in_place(key: &[u8], nonce: [u8; 12], buffer: &mut [u8]) -> Tag {
        let key = aead::UnboundKey::new(Self::algo(), &key).expect("bad key");
        let key = aead::LessSafeKey::new(key);

        let tag = key
            .seal_in_place_separate_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                buffer,
            )
            .unwrap();

        let mut t = Tag::default();
        t.copy_from_slice(tag.as_ref());
        t
    }

    fn decrypt_in_place(key: &[u8], nonce: [u8; 12], tag: Tag, buffer: &mut [u8]) {
        let key = aead::UnboundKey::new(Self::algo(), &key).expect("bad key");
        let key = aead::LessSafeKey::new(key);

        let size = buffer.len() - tag.len();

        buffer[size..].copy_from_slice(&tag);

        key.open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::empty(),
            buffer,
        )
        .expect("open failed");
    }
}

impl<A> CryptoProvider for A
where
    A: AeadProvider + Random,
{
    #[inline]
    fn tag_len(&self) -> usize {
        A::algo().tag_len()
    }

    fn encrypt_chunk(&self, object: &WriteObject, hash: &CryptoDigest, data: &mut [u8]) -> Tag {
        debug_assert_eq!(hash.len(), A::algo().key_len());
        let key = derive_chunk_key(&self.key(), hash);
        A::encrypt_in_place(&key, get_chunk_nonce(&object.id, data.len() as u32), data)
    }

    fn encrypt_object(&self, object: &mut WriteObject) {
        let capacity = object.capacity();

        let tag = A::encrypt_in_place(
            &self.key(),
            get_object_nonce(&object.id),
            &mut object.buffer.as_mut()[..capacity],
        );

        object.buffer.as_mut()[capacity..].copy_from_slice(&tag);
    }

    fn decrypt_chunk<T: AsRef<[u8]>>(
        &self,
        mut target: &mut [u8],
        o: &Object<T>,
        chunk: &ChunkPointer,
    ) -> usize {
        let size = chunk.size as usize;

        debug_assert_eq!(chunk.hash.len(), A::algo().key_len());
        assert_eq!(target.len(), size + chunk.tag.len());

        let key = derive_chunk_key(&self.key(), &chunk.hash);

        let start = chunk.offs as usize;
        let end = start + size;

        target[..size].copy_from_slice(&o.buffer.as_ref()[start..end]);

        A::decrypt_in_place(
            &key,
            get_chunk_nonce(&o.id, chunk.size),
            chunk.tag,
            &mut target,
        );

        size
    }

    fn decrypt_object<T: AsRef<[u8]>>(&self, o: &Object<T>) -> Vec<u8> {
        let mut data = o.buffer.as_ref().to_vec();
        let mut tag = Tag::default();
        tag.copy_from_slice(&data[data.len() - self.tag_len()..]);

        A::decrypt_in_place(&self.key(), get_object_nonce(&o.id), tag, &mut data);
        data.truncate(data.len() - self.tag_len());

        data
    }

    fn decrypt_object_into<T: AsRef<[u8]>>(&self, output: &mut [u8], o: &Object<T>) {
        let data = o.buffer.as_ref();
        let mut tag = Tag::default();
        tag.copy_from_slice(&data[data.len() - self.tag_len()..]);

        output[..data.len()].copy_from_slice(data);

        A::decrypt_in_place(&self.key(), get_object_nonce(&o.id), tag, output);
    }
}

#[inline]
fn derive_chunk_key(key_src: &[u8], hash: &CryptoDigest) -> Zeroizing<Vec<u8>> {
    let mut key = key_src.to_vec();
    for i in 0..key.len() {
        key[i] ^= hash[i];
    }
    Zeroizing::new(key)
}

#[inline]
fn get_object_nonce(object_id: &ObjectId) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&object_id.as_ref()[..12]);
    nonce
}

#[inline]
fn get_chunk_nonce(object_id: &ObjectId, data_size: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&object_id.as_ref()[..12]);

    let size = data_size.to_le_bytes();
    for i in 0..size.len() {
        nonce[i] ^= size[i];
    }
    // nonce[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());

    nonce
}

fn derive_argon2(salt: &[u8], password: &[u8]) -> Result<Vec<u8>, KeyError> {
    let mut outbuf = vec![0; crypto_kdf_KEYBYTES as usize];

    assert!(salt.len() == crypto_pwhash_SALTBYTES as usize);
    assert!(password.len() >= crypto_pwhash_PASSWD_MIN as usize);

    unsafe {
        if crypto_pwhash(
            outbuf.as_mut_ptr(),
            outbuf.len().try_into().unwrap(),
            password.as_ptr() as *const c_char,
            password.len().try_into().unwrap(),
            salt.as_ptr(),
            crypto_pwhash_opslimit_interactive().try_into().unwrap(),
            crypto_pwhash_memlimit_interactive(),
            crypto_pwhash_alg_default(),
        ) != 0
        {
            return Err(KeyError);
        }
    }

    Ok(outbuf)
}

fn derive_subkey(key: &[u8], subkey_id: u64, ctx: &[u8]) -> Result<Vec<u8>, KeyError> {
    let mut outbuf = vec![0; CRYPTO_DIGEST_SIZE];

    assert!(key.len() == crypto_kdf_KEYBYTES as usize);
    assert!(ctx.len() == 8);

    unsafe {
        if crypto_kdf_derive_from_key(
            outbuf.as_mut_ptr(),
            outbuf.len().try_into().unwrap(),
            subkey_id,
            ctx.as_ptr() as *const c_char,
            key.as_ptr(),
        ) != 0
        {
            return Err(KeyError);
        }
    }

    Ok(outbuf)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_chacha_primitives() {
        use super::{AeadProvider, ChaCha20Poly1305};

        let key = b"abcdef1234567890abcdef1234567890";
        let nonce = b"1234567890ab";
        let cleartext = "the quick brown fox jumps over the lazy crab";
        let mut buf = cleartext.as_bytes().to_vec();
        let tag = ChaCha20Poly1305::encrypt_in_place(key, *nonce, &mut buf);

        buf.resize(buf.len() + tag.len(), 0);
        ChaCha20Poly1305::decrypt_in_place(key, *nonce, tag, &mut buf);

        assert_eq!(&buf[..cleartext.len()], cleartext.as_bytes());
    }
}

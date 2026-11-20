//! The Standard security handler opened with the empty user password, as
//! PDFium opens a PDF for upstream when no password is given
//! (`CPDF_SecurityHandler`, `CPDF_CryptoHandler`; ISO 32000-1 7.6.3 and 7.6.4,
//! ISO 32000-2 7.6.4.3.4 for revision 6). Only decryption is here: the page
//! count reads an object stream, never writes one.

use aes::cipher::{Array, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use aes::{Aes128, Aes256};
use md5::{Digest, Md5};
use sha2::{Sha256, Sha384, Sha512};

/// The bytes a password is padded with to 32 (`kDefaultPasscode`): the whole
/// of the empty password.
const PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

/// How the document's streams are encrypted (`CPDF_CryptoHandler::Cipher`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cipher {
    Rc4,
    Aes,
}

/// What the encryption dictionary holds for the password check.
pub(super) struct Standard<'a> {
    /// `/R`.
    pub(super) revision: i64,
    /// The file key's length in bytes, from `/V`, `/Length` and the crypt
    /// filter.
    pub(super) key_len: usize,
    /// `/O`, `/U`, `/UE` and `/Perms`.
    pub(super) owner: &'a [u8],
    pub(super) user: &'a [u8],
    pub(super) user_key: &'a [u8],
    pub(super) perms: &'a [u8],
    /// `/P` as PDFium reads it into its key (`GetIntegerFor("P")`, 0 when
    /// absent) and into its permissions (`GetIntegerFor("P", -1)`).
    pub(super) p_key: i64,
    pub(super) p_permissions: i64,
    /// The first string of the trailer's `/ID`.
    pub(super) file_id: &'a [u8],
    /// `/EncryptMetadata`, true when absent.
    pub(super) encrypt_metadata: bool,
}

/// A document's decryption: its cipher and file key.
pub(super) struct Crypt {
    cipher: Cipher,
    key: Vec<u8>,
}

impl Crypt {
    /// The decryption the empty user password opens (`CheckPassword` with
    /// no owner password), or `None` when the document has another user
    /// password, as PDFium refuses it.
    pub(super) fn open(cipher: Cipher, dict: &Standard) -> Option<Self> {
        let mut key = if dict.revision >= 5 {
            aes256_key(dict)?
        } else {
            rc4_key(dict, false).or_else(|| rc4_key(dict, true))?
        };
        // The crypto handler takes the key's first `key_len` bytes.
        key.truncate(dict.key_len);
        Some(Self { cipher, key })
    }

    /// The data of stream object `id` generation `generation`, decrypted
    /// (`DecryptStart`, `DecryptStream`, `DecryptFinish`).
    pub(super) fn decrypt(&self, id: u64, generation: u64, mut data: Vec<u8>) -> Vec<u8> {
        if self.cipher == Cipher::Aes && self.key.len() == 32 {
            return aes_cbc_decrypt(&self.key, &data);
        }
        // The object's own key: the file key with the object number's low
        // three bytes and the generation's low two, hashed.
        let mut md5 = Md5::new();
        md5.update(&self.key);
        md5.update(&id.to_le_bytes()[..3]);
        md5.update(&generation.to_le_bytes()[..2]);
        if self.cipher == Cipher::Aes {
            md5.update(b"sAlT");
            return aes_cbc_decrypt(&md5.finalize(), &data);
        }
        let key = md5.finalize();
        rc4(&key[..(self.key.len() + 5).min(key.len())], &mut data);
        data
    }
}

/// Revisions 2 to 4: the file key the empty password gives
/// (`CalcEncryptKey`), when `/U` confirms it (`CheckUserPassword`).
/// `ignore_metadata` leaves `/EncryptMetadata false` out of the key, PDFium's
/// second try.
fn rc4_key(dict: &Standard, ignore_metadata: bool) -> Option<Vec<u8>> {
    let mut md5 = Md5::new();
    md5.update(PADDING);
    md5.update(dict.owner);
    // The low 32 bits, as PDFium's `uint32_t` holds them.
    md5.update(&dict.p_key.to_le_bytes()[..4]);
    md5.update(dict.file_id);
    if !ignore_metadata && dict.revision >= 3 && !dict.encrypt_metadata {
        md5.update([0xff; 4]);
    }
    let mut digest: [u8; 16] = md5.finalize().into();
    let len = dict.key_len.min(digest.len());
    if dict.revision >= 3 {
        for _ in 0..50 {
            digest = Md5::digest(&digest[..len]).into();
        }
    }
    let mut key = vec![0; dict.key_len];
    key[..len].copy_from_slice(&digest[..len]);
    let user = dict.user.get(..16)?;
    let confirmed = if dict.revision == 2 {
        let mut test = PADDING;
        rc4(&key, &mut test);
        test[..16] == *user
    } else {
        // `/U` undone: twenty RC4 passes, the key XORed with 19 down to 0.
        let mut test = [0_u8; 32];
        let n = dict.user.len().min(test.len());
        test[..n].copy_from_slice(&dict.user[..n]);
        for i in (0..=19_u8).rev() {
            let round: Vec<u8> = key.iter().map(|b| b ^ i).collect();
            rc4(&round, &mut test);
        }
        let mut md5 = Md5::new();
        md5.update(PADDING);
        md5.update(dict.file_id);
        test[..16] == md5.finalize()[..]
    };
    confirmed.then_some(key)
}

/// Revisions 5 and 6: the file key `/UE` holds, when `/U` confirms the empty
/// password and `/Perms` the permissions (`AES256_CheckPassword`).
fn aes256_key(dict: &Standard) -> Option<Vec<u8>> {
    if dict.owner.len() < 48 || dict.user.len() < 48 {
        return None;
    }
    let hash = |salt: &[u8]| -> [u8; 32] {
        if dict.revision >= 6 {
            revision6_hash(salt)
        } else {
            Sha256::digest(salt).into()
        }
    };
    // `/U`: the hash, its validation salt, its key salt.
    if hash(&dict.user[32..40]) != dict.user[..32] {
        return None;
    }
    let intermediate = hash(&dict.user[40..48]);
    let mut key = dict.user_key.get(..32)?.to_vec();
    cbc_decrypt(&intermediate, [0; 16], &mut key)?;
    if dict.perms.is_empty() {
        return None;
    }
    let mut perms = [0_u8; 16];
    let n = dict.perms.len().min(perms.len());
    perms[..n].copy_from_slice(&dict.perms[..n]);
    cbc_decrypt(&key, [0; 16], &mut perms)?;
    let confirmed = perms[9..12] == *b"adb"
        && perms[..4] == dict.p_permissions.to_le_bytes()[..4]
        && (perms[8] == b'F' || dict.encrypt_metadata);
    confirmed.then_some(key)
}

/// ISO 32000-2's hash of revision 6 (Algorithm 2.B, `Revision6_Hash`) for the
/// empty password and no owner data: SHA-256 of the salt, then rounds of
/// AES-128-CBC over 64 copies of the last hash, each rehashed with SHA-256,
/// -384 or -512 as the first 16 bytes' sum modulo 3 picks, until round 64 and
/// a round whose last byte is at most its number minus 32.
fn revision6_hash(salt: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::digest(salt).to_vec();
    let mut round = 0_u32;
    loop {
        let mut data = hash.repeat(64);
        let mut iv = [0_u8; 16];
        iv.copy_from_slice(&hash[16..32]);
        if let Ok(aes) = Aes128::new_from_slice(&hash[..16]) {
            for block in data.as_chunks_mut::<16>().0 {
                for (byte, chain) in block.iter_mut().zip(iv) {
                    *byte ^= chain;
                }
                let mut array = Array::from(*block);
                aes.encrypt_block(&mut array);
                *block = array.into();
                iv = *block;
            }
        }
        let last = data.last().copied().unwrap_or(0);
        hash = match data[..16].iter().map(|b| u32::from(*b)).sum::<u32>() % 3 {
            0 => Sha256::digest(&data).to_vec(),
            1 => Sha384::digest(&data).to_vec(),
            _ => Sha512::digest(&data).to_vec(),
        };
        round += 1;
        if round >= 64 && round - 32 >= u32::from(last) {
            break;
        }
    }
    let mut out = [0_u8; 32];
    out.copy_from_slice(&hash[..32]);
    out
}

/// A stream's data decrypted as PDFium's AES context does: the first 16 bytes
/// are the IV, whole blocks are deciphered, and the last block's padding is
/// dropped only when the data ends on a block boundary (a final byte of 16 or
/// more drops that whole block). Data shorter than one block is empty.
fn aes_cbc_decrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    let Some((iv, body)) = data.split_first_chunk::<16>() else {
        return Vec::new();
    };
    let (blocks, ragged) = body.as_chunks::<16>();
    let mut out = blocks.as_flattened().to_vec();
    if cbc_decrypt(key, *iv, &mut out).is_none() {
        return Vec::new();
    }
    if ragged.is_empty()
        && let Some(&pad) = out.last()
    {
        let drop = if pad < 16 { usize::from(pad) } else { 16 };
        out.truncate(out.len() - drop);
    }
    out
}

/// Deciphers `data`'s whole blocks in place as AES-CBC, with a 16- or 32-byte
/// key; `None` for another key length.
fn cbc_decrypt(key: &[u8], mut iv: [u8; 16], data: &mut [u8]) -> Option<()> {
    let (aes128, aes256) = match key.len() {
        16 => (Some(Aes128::new_from_slice(key).ok()?), None),
        32 => (None, Some(Aes256::new_from_slice(key).ok()?)),
        _ => return None,
    };
    for block in data.as_chunks_mut::<16>().0 {
        let cipher_text = *block;
        let mut array = Array::from(*block);
        if let Some(aes) = &aes128 {
            aes.decrypt_block(&mut array);
        } else if let Some(aes) = &aes256 {
            aes.decrypt_block(&mut array);
        }
        *block = array.into();
        for (byte, chain) in block.iter_mut().zip(iv) {
            *byte ^= chain;
        }
        iv = cipher_text;
    }
    Some(())
}

/// RC4 over `data` in place with `key` (`CRYPT_ArcFourCryptBlock`).
fn rc4(key: &[u8], data: &mut [u8]) {
    if key.is_empty() {
        return;
    }
    let mut state: Vec<u8> = (0..=u8::MAX).collect();
    let mut j = 0_u8;
    for i in 0..state.len() {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, usize::from(j));
    }
    let (mut i, mut j) = (0_u8, 0_u8);
    for byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(state[usize::from(i)]);
        state.swap(usize::from(i), usize::from(j));
        *byte ^= state[usize::from(state[usize::from(i)].wrapping_add(state[usize::from(j)]))];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic RC4 test vectors.
    #[rstest::rstest]
    #[case(b"Key", b"Plaintext", "bbf316e8d940af0ad3")]
    #[case(b"Wiki", b"pedia", "1021bf0420")]
    #[case(b"Secret", b"Attack at dawn", "45a01f645fc35b383552544b9bf5")]
    fn rc4_matches_its_test_vectors(
        #[case] key: &[u8],
        #[case] plain: &[u8],
        #[case] cipher: &str,
    ) {
        let mut data = plain.to_vec();
        rc4(key, &mut data);
        let expected: Vec<u8> = (0..cipher.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&cipher[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(data, expected);
    }

    /// PDFium's stream padding: dropped on a block boundary only, the whole
    /// last block for a final byte of 16 or more, and nothing from less than
    /// one block.
    #[test]
    fn aes_stream_padding_follows_pdfium() {
        let key = [7_u8; 16];
        let iv = [9_u8; 16];
        let encrypt = |plain: &[u8]| {
            let aes = Aes128::new_from_slice(&key).unwrap();
            let mut chain = iv;
            let mut out = iv.to_vec();
            for block in plain.as_chunks::<16>().0 {
                let mut block = *block;
                for (b, c) in block.iter_mut().zip(chain) {
                    *b ^= c;
                }
                let mut array = Array::from(block);
                aes.encrypt_block(&mut array);
                chain = array.into();
                out.extend_from_slice(&chain);
            }
            out
        };
        let mut padded = b"hello, pdf".to_vec();
        padded.resize(16, 6);
        assert_eq!(aes_cbc_decrypt(&key, &encrypt(&padded)), b"hello, pdf");
        let mut full = [b'x'; 16].to_vec();
        full.resize(32, 16);
        assert_eq!(aes_cbc_decrypt(&key, &encrypt(&full)), [b'x'; 16]);
        // A ragged tail: the whole blocks before it are kept as they are.
        let mut ragged = encrypt(&padded);
        ragged.push(0);
        assert_eq!(aes_cbc_decrypt(&key, &ragged), padded);
        assert_eq!(aes_cbc_decrypt(&key, &iv[..15]), Vec::<u8>::new());
        assert_eq!(
            aes_cbc_decrypt(&[1; 5], &encrypt(&padded)),
            Vec::<u8>::new()
        );
    }
}

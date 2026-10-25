//! Decrypting RAR headers, for archives made with "Encrypt file names"
//! (`rar -hp`), exactly as UnRAR's `crypt3.cpp` and `crypt5.cpp` do it.
//!
//! RAR 5 encrypts every header after the archive encryption header with
//! AES-256-CBC, under a key derived from the password with
//! PBKDF2-HMAC-SHA256, and puts a fresh 16-byte IV in front of each one.
//! RAR 4 encrypts every header after the main header with AES-128-CBC, and
//! puts an 8-byte salt in front of each one; the key and the IV both come
//! from the password and that salt through RAR 3's own SHA-1 derivation.
//! Either way a header is padded to a whole number of 16-byte blocks.

use aes::{Aes128, Aes256};
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use sha2::Sha256;

pub(super) const BLOCK: u64 = 16;

/// The AES-256 key of a RAR 5 archive's headers: PBKDF2-HMAC-SHA256 of the
/// password, as UTF-8, over `2^count` iterations (`CryptData::SetKey50`).
pub(super) fn key5(password: &str, salt: &[u8; 16], count: u8) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, 1u32 << count, &mut key);
    key
}

/// Decrypts whole blocks of a RAR 5 header in place.
pub(super) fn decrypt5(key: &[u8; 32], iv: &[u8], buf: &mut [u8]) {
    cbc::Decryptor::<Aes256>::new_from_slices(key, iv)
        .expect("a 32-byte key and a 16-byte IV")
        .decrypt_padded::<NoPadding>(buf)
        .expect("whole blocks");
}

/// Decrypts whole blocks of a RAR 4 header in place.
pub(super) fn decrypt4(key: &[u8; 16], iv: &[u8; 16], buf: &mut [u8]) {
    cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
        .expect("a 16-byte key and a 16-byte IV")
        .decrypt_padded::<NoPadding>(buf)
        .expect("whole blocks");
}

/// The AES-128 key and IV of a RAR 4 header with the given salt
/// (`CryptData::SetKey30`). `password` is the password as UnRAR holds it,
/// in 16-bit little-endian units. 2^18 rounds each hash the password and
/// salt, then the round number; the IV takes one byte of an intermediate
/// digest every 2^14 rounds, and the key is the final digest.
pub(super) fn key4(password: &[u8], salt: &[u8; 8]) -> ([u8; 16], [u8; 16]) {
    const ROUNDS: u32 = 0x40000;
    let mut raw = password.to_vec();
    raw.extend_from_slice(salt);
    let mut sha = Sha1::new();
    let mut iv = [0u8; 16];
    for i in 0..ROUNDS {
        sha.update(&mut raw, true);
        sha.update(&mut [i as u8, (i >> 8) as u8, (i >> 16) as u8], false);
        if i % (ROUNDS / 16) == 0 {
            let digest = sha.clone().finish();
            iv[(i / (ROUNDS / 16)) as usize] = digest[4] as u8;
        }
    }
    let digest = sha.finish();
    let mut key = [0u8; 16];
    for (i, word) in digest[..4].iter().enumerate() {
        key[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    (key, iv)
}

/// SHA-1 as UnRAR's `sha1.cpp` computes it, including the quirk of RAR 2.9
/// that RAR 3 and 4 keep for compatibility: hashing a whole 64-byte block
/// straight from the input overwrites that block with the last sixteen
/// words of its message schedule. The key derivation hashes the same buffer
/// 2^18 times, so with a password of 29 or more characters, long enough for
/// such a block, every later round hashes what the quirk wrote.
#[derive(Clone)]
struct Sha1 {
    state: [u32; 5],
    count: u64,
    buf: [u8; 64],
}

impl Sha1 {
    fn new() -> Self {
        Self {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            count: 0,
            buf: [0; 64],
        }
    }

    /// Hashes one block and returns the last sixteen words of its message
    /// schedule, which UnRAR leaves in its workspace.
    fn compress(state: &mut [u32; 5], block: &[u8]) -> [u32; 16] {
        let mut w = [0u32; 16];
        for (i, word) in w.iter_mut().enumerate() {
            *word = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().expect("four bytes"));
        }
        let [mut a, mut b, mut c, mut d, mut e] = *state;
        for i in 0..80 {
            if i >= 16 {
                w[i & 15] = (w[(i + 13) & 15] ^ w[(i + 8) & 15] ^ w[(i + 2) & 15] ^ w[i & 15])
                    .rotate_left(1);
            }
            let (f, k) = match i {
                0..20 => ((b & c) | (!b & d), 0x5A827999),
                20..40 => (b ^ c ^ d, 0x6ED9EBA1),
                40..60 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i & 15]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d, e]) {
            *s = s.wrapping_add(v);
        }
        w
    }

    /// `sha1_process`, or with `rar29` `sha1_process_rar29`, which writes
    /// back into `data`.
    fn update(&mut self, data: &mut [u8], rar29: bool) {
        let mut j = (self.count & 63) as usize;
        self.count += data.len() as u64;
        let mut i = 0;
        if j + data.len() > 63 {
            i = 64 - j;
            self.buf[j..].copy_from_slice(&data[..i]);
            Self::compress(&mut self.state, &self.buf);
            while i + 63 < data.len() {
                let w = Self::compress(&mut self.state, &data[i..i + 64]);
                if rar29 {
                    for (k, word) in w.iter().enumerate() {
                        data[i + k * 4..i + k * 4 + 4].copy_from_slice(&word.to_le_bytes());
                    }
                }
                i += 64;
            }
            j = 0;
        }
        self.buf[j..j + data.len() - i].copy_from_slice(&data[i..]);
    }

    /// The digest as UnRAR's `sha1_done` gives it: the five state words.
    fn finish(mut self) -> [u32; 5] {
        let bits = self.count.wrapping_mul(8);
        let mut pos = (self.count & 63) as usize;
        self.buf[pos] = 0x80;
        pos += 1;
        if pos > 56 {
            self.buf[pos..].fill(0);
            Self::compress(&mut self.state, &self.buf);
            pos = 0;
        }
        self.buf[pos..56].fill(0);
        self.buf[56..].copy_from_slice(&bits.to_be_bytes());
        Self::compress(&mut self.state, &self.buf);
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(words: [u32; 5]) -> String {
        words.iter().map(|w| format!("{w:08x}")).collect()
    }

    fn digest(data: &[u8]) -> String {
        let mut sha = Sha1::new();
        // In pieces, to cross block boundaries both ways.
        for piece in data.chunks(37) {
            sha.update(&mut piece.to_vec(), false);
        }
        hex(sha.finish())
    }

    /// Without the quirk this is plain SHA-1 (FIPS 180 test vectors).
    #[test]
    fn plain_sha1_matches_the_standard() {
        assert_eq!(digest(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(digest(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            digest(&b"a".repeat(1000)),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    /// The quirk changes only a block hashed straight from the input, and
    /// leaves the digest of that call alone.
    #[test]
    fn the_rar29_quirk_rewrites_whole_blocks_of_the_input() {
        let mut short = b"x".repeat(60);
        let mut sha = Sha1::new();
        sha.update(&mut short, true);
        assert_eq!(short, b"x".repeat(60));

        let mut long = b"y".repeat(200);
        let mut quirky = Sha1::new();
        quirky.update(&mut long, true);
        assert_ne!(long, b"y".repeat(200));
        assert_eq!(hex(quirky.finish()), digest(&b"y".repeat(200)));
    }
}

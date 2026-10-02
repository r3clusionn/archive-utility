//! Compressing and decompressing one chunk.

use std::io::{Read, Write};

use crate::format::Codec;

pub struct Encoded {
    /// The codec actually used: `None` when compression did not make the chunk smaller.
    pub codec: Codec,
    pub payload: Vec<u8>,
    /// BLAKE3 of the uncompressed bytes.
    pub digest: [u8; 32],
    /// CRC-32 of the stored bytes. The digest alone would miss a flipped bit in the unused tail of
    /// a compressed stream, which decompresses to the same data.
    pub payload_crc: u32,
    pub raw_len: u64,
}

/// Hashes and compresses a chunk. Incompressible data is stored as it is.
pub fn encode_chunk(raw: Vec<u8>, codec: Codec, level: u32) -> Encoded {
    let digest = *blake3::hash(&raw).as_bytes();
    let raw_len = raw.len() as u64;
    let compressed = match codec {
        Codec::None => None,
        Codec::Lz4 => Some(lz4_flex::block::compress(&raw)),
        Codec::Deflate => {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::with_capacity(raw.len() / 2 + 64), flate2::Compression::new(level.min(9)));
            enc.write_all(&raw).ok().and_then(|_| enc.finish().ok())
        }
    };
    let (codec, payload) = match compressed {
        Some(c) if c.len() < raw.len() => (codec, c),
        _ => (Codec::None, raw),
    };
    let payload_crc = crate::format::crc32(&[&payload]);
    Encoded { codec, payload, digest, payload_crc, raw_len }
}

/// Checks a chunk's stored bytes against their CRC, decompresses it and checks the result against
/// its digest. `max_raw` bounds the output size.
pub fn decode_chunk(codec: u8, payload: &[u8], raw_len: u64, max_raw: u64, digest: &[u8; 32], payload_crc: u32) -> Result<Vec<u8>, String> {
    if crate::format::crc32(&[payload]) != payload_crc {
        return Err("the chunk's stored bytes do not match their checksum".to_string());
    }
    if raw_len > max_raw {
        return Err(format!("a chunk claims {raw_len} bytes, more than the archive's chunk size {max_raw}"));
    }
    let raw = match Codec::from_u8(codec) {
        Some(Codec::None) => {
            if payload.len() as u64 != raw_len {
                return Err("a stored chunk's length does not match its header".to_string());
            }
            payload.to_vec()
        }
        Some(Codec::Lz4) => lz4_flex::block::decompress(payload, raw_len as usize).map_err(|e| format!("lz4 data is corrupt: {e}"))?,
        Some(Codec::Deflate) => {
            let mut out = Vec::with_capacity(raw_len as usize);
            flate2::read::DeflateDecoder::new(payload).take(raw_len + 1).read_to_end(&mut out).map_err(|e| format!("deflate data is corrupt: {e}"))?;
            out
        }
        None => return Err(format!("unknown codec {codec}")),
    };
    if raw.len() as u64 != raw_len {
        return Err(format!("a chunk decompressed to {} bytes instead of {raw_len}", raw.len()));
    }
    if blake3::hash(&raw).as_bytes() != digest {
        return Err("the chunk's checksum does not match its data".to_string());
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> Vec<u8> {
        b"the quick brown fox jumps over the lazy dog. ".iter().copied().cycle().take(n).collect()
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    #[test]
    fn every_codec_round_trips() {
        for codec in [Codec::None, Codec::Lz4, Codec::Deflate] {
            for data in [Vec::new(), vec![0u8], text(10_000), noise(10_000), vec![7u8; 100_000]] {
                let e = encode_chunk(data.clone(), codec, 6);
                let back = decode_chunk(e.codec as u8, &e.payload, e.raw_len, 1 << 20, &e.digest, e.payload_crc).unwrap();
                assert_eq!(back, data, "{codec:?} len {}", data.len());
            }
        }
    }

    #[test]
    fn compressible_data_shrinks_and_incompressible_data_is_stored() {
        let t = encode_chunk(text(100_000), Codec::Lz4, 6);
        assert_eq!(t.codec, Codec::Lz4);
        assert!(t.payload.len() < 20_000);
        let n = encode_chunk(noise(100_000), Codec::Deflate, 6);
        assert_eq!(n.codec, Codec::None, "random bytes are stored as they are");
        assert_eq!(n.payload.len(), 100_000);
    }

    #[test]
    fn damage_is_always_noticed() {
        for codec in [Codec::None, Codec::Lz4, Codec::Deflate] {
            let e = encode_chunk(text(5000), codec, 6);
            for bit in 0..e.payload.len() * 8 {
                let (i, mask) = (bit / 8, 1u8 << (bit % 8));
                let mut p = e.payload.clone();
                p[i] ^= mask;
                assert!(decode_chunk(e.codec as u8, &p, e.raw_len, 1 << 20, &e.digest, e.payload_crc).is_err(), "{codec:?} byte {i} bit {mask}");
            }
            // truncated payload
            assert!(decode_chunk(e.codec as u8, &e.payload[..e.payload.len() - 1], e.raw_len, 1 << 20, &e.digest, e.payload_crc).is_err());
            // wrong digest
            assert!(decode_chunk(e.codec as u8, &e.payload, e.raw_len, 1 << 20, &[0; 32], e.payload_crc).is_err());
        }
    }

    #[test]
    fn an_oversized_claim_is_refused_without_allocating() {
        assert!(decode_chunk(1, &[0; 4], u64::MAX, 1 << 20, &[0; 32], 0).is_err());
        assert!(decode_chunk(9, &[], 0, 1 << 20, &[0; 32], crate::format::crc32(&[&[]])).is_err());
    }
}

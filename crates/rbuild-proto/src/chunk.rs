//! Content-defined chunking and zstd helpers for delta sync.
//!
//! Large files are split into variable-length chunks with boundaries chosen
//! by a rolling hash of the content (a Rabin-style fingerprint). Because
//! boundaries follow content rather than fixed offsets, an insertion early in
//! a file only re-chunks the region around the edit instead of shifting every
//! subsequent chunk — so a one-byte change in a multi-GB file transfers one
//! small chunk.

use serde::{Deserialize, Serialize};

use crate::hash::Hash;

/// Target average chunk size. The mask below is `2^13 - 1`, so boundaries
/// fall roughly every 8 KiB on random data; min/max clamp the tails.
const AVG_BITS: u32 = 13;
const MIN_CHUNK: usize = 2 * 1024;
const MAX_CHUNK: usize = 64 * 1024;

/// One chunk's strong hash and length — enough for the receiver to say which
/// chunks it already has and which it needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Hash,
    pub len: u32,
}

/// Splits `data` into content-defined chunks, returning each chunk's offset.
pub fn chunk(data: &[u8]) -> Vec<(usize, usize)> {
    let mask = (1u64 << AVG_BITS) - 1;
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut hash: u64 = 0;
    let mut i = 0;

    while i < data.len() {
        // A cheap rolling fingerprint: it need not be cryptographic, only
        // well-distributed enough to place boundaries pseudo-randomly.
        hash = (hash << 1).wrapping_add(data[i] as u64);
        let len = i - start + 1;

        let boundary = len >= MIN_CHUNK && (hash & mask) == mask;
        if boundary || len >= MAX_CHUNK {
            chunks.push((start, i + 1));
            start = i + 1;
            hash = 0;
        }
        i += 1;
    }
    if start < data.len() {
        chunks.push((start, data.len()));
    }
    chunks
}

/// Produces the chunk reference list (the "signature") for a file's bytes.
pub fn signature(data: &[u8]) -> Vec<ChunkRef> {
    chunk(data)
        .into_iter()
        .map(|(s, e)| ChunkRef {
            hash: Hash::of(&data[s..e]),
            len: (e - s) as u32,
        })
        .collect()
}

pub fn compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::encode_all(data, 3)
}

pub fn decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::decode_all(data)
}

/// Builds a delta that reconstructs `new` from a file whose content-defined
/// chunk signature is `base_sig` (the daemon's existing copy). Returns the
/// op list plus the literal bytes each `Data` op refers to, in order.
///
/// Because both sides chunk by content, an unchanged region produces the same
/// chunk hashes on both ends and becomes a `Copy`; only genuinely new chunks
/// ship as literals.
pub fn make_delta(
    new: &[u8],
    base_sig: &[ChunkRef],
) -> (Vec<crate::proto::DeltaOpKind>, Vec<u8>) {
    use crate::proto::DeltaOpKind;
    use std::collections::HashMap;

    // Map each base chunk hash to its (offset, len) in the old file.
    let mut index: HashMap<Hash, (u64, u32)> = HashMap::with_capacity(base_sig.len());
    let mut offset = 0u64;
    for c in base_sig {
        index.entry(c.hash).or_insert((offset, c.len));
        offset += c.len as u64;
    }

    let mut ops = Vec::new();
    let mut literals = Vec::new();
    for (s, e) in chunk(new) {
        let piece = &new[s..e];
        let h = Hash::of(piece);
        match index.get(&h) {
            Some(&(off, len)) if len as usize == piece.len() => {
                ops.push(DeltaOpKind::Copy { offset: off, len });
            }
            _ => {
                ops.push(DeltaOpKind::Data { len: piece.len() as u32 });
                literals.extend_from_slice(piece);
            }
        }
    }
    (ops, literals)
}

/// Reconstructs a file from delta ops, the daemon's `base` bytes, and the
/// stream of literal bytes the `Data` ops consume in order.
pub fn apply_delta(
    base: &[u8],
    ops: &[crate::proto::DeltaOpKind],
    literals: &[u8],
) -> std::io::Result<Vec<u8>> {
    use crate::proto::DeltaOpKind;
    let mut out = Vec::new();
    let mut lit_pos = 0usize;
    for op in ops {
        match op {
            DeltaOpKind::Copy { offset, len } => {
                let start = *offset as usize;
                let end = start + *len as usize;
                if end > base.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "delta Copy out of bounds",
                    ));
                }
                out.extend_from_slice(&base[start..end]);
            }
            DeltaOpKind::Data { len } => {
                let end = lit_pos + *len as usize;
                if end > literals.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "delta Data out of bounds",
                    ));
                }
                out.extend_from_slice(&literals[lit_pos..end]);
                lit_pos = end;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_are_stable_under_insertion() {
        // A change near the front should leave most later chunk boundaries
        // intact — the property the whole delta scheme relies on.
        let mut data = vec![0u8; 200_000];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2654435761) >> 13) as u8;
        }
        let sig_a = signature(&data);

        let mut edited = data.clone();
        edited.splice(50..50, [42u8, 42, 42]); // insert 3 bytes near the front
        let sig_b = signature(&edited);

        let shared = sig_a
            .iter()
            .rev()
            .zip(sig_b.iter().rev())
            .take_while(|(a, b)| a.hash == b.hash)
            .count();
        // The tail of the file should re-sync identically.
        assert!(shared > sig_a.len() / 2, "shared tail chunks: {shared}");
    }

    #[test]
    fn roundtrip_compression() {
        let data = b"the quick brown fox".repeat(100);
        assert_eq!(decompress(&compress(&data).unwrap()).unwrap(), data);
    }

    #[test]
    fn delta_reconstructs_and_sends_little() {
        let mut base = vec![0u8; 500_000];
        for (i, b) in base.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2654435761) >> 13) as u8;
        }
        let base_sig = signature(&base);

        // Edit a few bytes in the middle of the file.
        let mut new = base.clone();
        new[250_000] ^= 0xFF;
        new[250_001] ^= 0xFF;

        let (ops, literals) = make_delta(&new, &base_sig);
        let rebuilt = apply_delta(&base, &ops, &literals).unwrap();
        assert_eq!(rebuilt, new, "delta must reconstruct exactly");

        // The literal payload should be a tiny fraction of the file — only the
        // chunk(s) around the edit, not the whole thing.
        assert!(
            literals.len() < new.len() / 10,
            "delta sent {} of {} bytes",
            literals.len(),
            new.len()
        );
    }
}

//! The `jqfb` machine profile — the jqft family's chunked binary image.
//!
//! STABILITY: INTERNAL/UNSTABLE. The chunk layout is pinned as far as it exists, but the format has NO external reader
//! and NO committed byte-level spec. Until fact schemas freeze, nobody should persist archives in this format expecting
//! cross-version stability; the encoder/decoder are the format, and they may change with the family.
//!
//! Layout (pinned): a 10-byte header (`b"jqfb"` magic + u16 LE version + u32 LE flags), then CHUNKS, then a footer
//! DIRECTORY that ends with an 8-byte footer length. A reader seeks to the last 8 bytes, reads the footer, and
//! validates every entry before touching any chunk. Each chunk is described by one directory entry (type, absolute
//! offset, byte length, blake3 digest).
//!
//! The high bit of a chunk type marks it ignorable. An unknown IGNORABLE chunk is skipped by v1 readers; an unknown
//! CRITICAL chunk refuses the file.
//!
//! v1 chunk types:
//!
//! - `NODE` (critical): the flattened PREORDER node table. One 9-byte entry per node: kind `u8`, `subtree_size` `u32`
//!   LE (the number of node entries the node's subtree occupies, self-inclusive — skip-a-value is arithmetic, the
//!   demand routes' native primitive), payload `u32` LE (a pool index, a child/member count, or 0). A node's subtree is
//!   contiguous by construction, and the reader validates the invariant exactly.
//! - `STRG` (critical): the deduplicated string/bytes pool — per entry a `u32` LE length and the raw bytes.
//! - `NUMB` (critical): the number pool mirroring the engine's `Number` categories exactly (integer text / decimal
//!   coefficient+scale / binary64 bits).
//! - `FACT` (critical when present): attached facts (name/attrs/content/attribute/comment/provenance payloads), one
//!   record per (node, fact). A missing FACT chunk is an empty fact table; a duplicate refuses.
//! - `PROV` (ignorable): the provenance header — producing codec + dialect + jqf version (provenance is a YES, as an
//!   ignorable chunk v1 readers skip for free). The decode surfaces it as a `.@provenance` fact on the root.
//! - `SOUR` (ignorable): the retained source bytes (conformance level 1 — spans and retained source; byte-identical
//!   re-emission authority).
//!
//! ## The splice policy
//!
//! `--edit` on jqfb preserves the user's BYTES: a changed scalar rewrites only the value's own bytes plus the
//! footer-directory words that name the changed chunks, and a container that gained or lost a member rewrites the
//! count-bearing NODE entries whose counts moved plus the same footer words. NODE entries carry the ITEM/PAIR COUNT
//! (`payload`) and the ENTRY SPAN (`subtree_size`), never a byte length, so a splice below a container changes that
//! container's byte size but not its counts — the ENCLOSING containers' entries are untouched by a leaf change.
//!
//! A jqfb scalar's value bytes sit in the STRG/NUMB POOLS (referenced by a pool index in the node entry), and every
//! chunk's position and blake3 digest is recorded in the FOOTER DIRECTORY at the file tail. Two consequences follow,
//! and they ARE the policy:
//!
//! 1. **A leaf splice's replacement is the tail from the changed item through EOF.** Replacing a scalar's pool entry
//!    changes that pool chunk's content (and, when the new entry's length differs, its length), which moves the digest
//!    of the pool chunk, the offset of every chunk after it, and — through the absolute-offset table — the footer
//!    directory itself. The one contiguous span that can carry all of that bookkeeping is the node's authored span,
//!    bound at decode as the tail from the node's table entry through the image end: the replacement rewrites the node
//!    entry (when the value's pool home or kind changed), the value's pool entry, and the footer words, and copies
//!    every byte between them VERBATIM. Nothing else in the image is re-encoded — the header, the node table's other
//!    entries, the pools' other entries, and every untouched chunk are byte-identical.
//! 2. **A structural splice rewrites the ONE container whose count moved plus the footer, and nothing above it.**
//!    Appending a member inserts the new member's node entries at the end of the container's subtree, re-derives the
//!    container's own `payload`/`subtree_size` and every ANCESTOR's `subtree_size` (each ancestor's span grew by the
//!    same node count), and appends any genuinely-new pool entries (the pools are deduplicated against the existing
//!    entries first). Removing a member cuts its node entries — the KEYTEXT entry plus the value's whole subtree for an
//!    object member, the item's subtree for an array item — and re-derives the same counts downward. Orphaned pool
//!    entries are harmless (the pools are dedup stores; an unreferenced entry is never read), so a removal never
//!    rewrites a pool chunk.
//!
//! Every splice is re-verified by the SDK's re-decode law (the patched image re-decodes with the footer and digest
//! checks applied, and its value must equal the program's output); a span the source contradicts — a node whose entry
//! is not where its span says, a pool that does not parse — declines to the whole-document floor, never wrong bytes.
//!
//! Read path: validate every offset, length, and count in the footer directory and the chunks against the file's actual
//! extent before use — a malformed file is a typed error, never a panic and never an out-of-bounds read (a `jqfb` file
//! is attacker-controlled input, and this is the one place in the format family where a bug is a security bug). Every
//! chunk's blake3 digest is verified against its payload before any byte is consumed.

use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, Severity};

/// The image's textual magic.
pub(crate) const MAGIC: &[u8; 4] = b"jqfb";
/// The v1 image version.
pub(crate) const VERSION: u16 = 1;
/// Header size: magic (4) + version (2) + flags (4).
pub(crate) const HEADER_LEN: usize = 10;
/// One footer directory entry: type u32 + offset u64 + length u64 + digest [u8; 32] = 52 bytes.
pub(crate) const FOOTER_ENTRY_LEN: usize = 52;
/// The footer's fixed words: entry count u64 + the trailing footer-length u64.
pub(crate) const FOOTER_FIXED_LEN: usize = 16;

/// The ignorable bit in a chunk type.
pub(crate) const CHUNK_IGNORABLE: u32 = 0x8000_0000;
/// The critical node-table chunk.
pub(crate) const CHUNK_NODE: u32 = 0x0000_0001;
/// The critical string/bytes pool chunk.
pub(crate) const CHUNK_STRG: u32 = 0x0000_0002;
/// The critical number pool chunk.
pub(crate) const CHUNK_NUMB: u32 = 0x0000_0003;
/// The critical attached-facts chunk.
pub(crate) const CHUNK_FACT: u32 = 0x0000_0004;
/// The ignorable provenance header chunk.
pub(crate) const CHUNK_PROV: u32 = 0x8000_0005;
/// The ignorable retained-source chunk.
pub(crate) const CHUNK_SOUR: u32 = 0x8000_0006;

/// One validated footer directory entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DirectoryEntry {
    pub(crate) chunk_type: u32,
    pub(crate) offset: usize,
    pub(crate) length: usize,
    pub(crate) digest: [u8; 32],
}

impl DirectoryEntry {
    pub(crate) fn is_ignorable(self) -> bool {
        self.chunk_type & CHUNK_IGNORABLE != 0
    }

    pub(crate) fn end(self) -> usize {
        self.offset.saturating_add(self.length)
    }
}

/// The footer directory parsed out of the file tail.
#[derive(Debug)]
pub(crate) struct Footer {
    pub(crate) entries: Vec<DirectoryEntry>,
}

/// Reads the footer directory from the file tail.
///
/// The footer is the LAST `footer_len` bytes of the file, and `footer_len` (a u64 LE word) is its own final word. Every
/// entry's offset/length is validated against the file extent here, BEFORE any chunk byte is read — this is the trust
/// boundary.
pub(crate) fn read_footer(bytes: &[u8]) -> Result<Footer, CodecError> {
    let file_len = bytes.len();
    if file_len < HEADER_LEN + FOOTER_FIXED_LEN + FOOTER_ENTRY_LEN {
        return Err(invalid("the file is too small to carry a footer directory"));
    }
    let footer_len = read_u64(bytes, file_len - 8).ok_or_else(|| invalid("truncated footer"))?;
    let footer_len = usize::try_from(footer_len).map_err(|_| invalid("footer length overflows"))?;
    // The footer occupies the file tail and must leave room for the header; a small image legitimately carries a footer
    // LARGER than its payload.
    if footer_len < FOOTER_FIXED_LEN || footer_len > file_len - HEADER_LEN {
        return Err(invalid("footer length lies outside the file"));
    }
    let footer_start = file_len - footer_len;
    let count = read_u64(bytes, footer_start).ok_or_else(|| invalid("truncated footer entry count"))?;
    let count = usize::try_from(count).map_err(|_| invalid("footer entry count overflows"))?;
    if footer_len != FOOTER_FIXED_LEN.saturating_add(count.saturating_mul(FOOTER_ENTRY_LEN)) {
        return Err(invalid("footer length does not match its entry count"));
    }
    let mut entries: Vec<DirectoryEntry> = Vec::with_capacity(count);
    let mut cursor = footer_start + 8;
    for _ in 0..count {
        let chunk_type = read_u32(bytes, cursor).ok_or_else(|| invalid("truncated entry type"))?;
        let offset = read_u64(bytes, cursor + 4).ok_or_else(|| invalid("truncated entry offset"))?;
        let length = read_u64(bytes, cursor + 12).ok_or_else(|| invalid("truncated entry length"))?;
        let mut digest = [0u8; 32];
        let digest_start = cursor + 20;
        if digest_start + 32 > bytes.len() {
            return Err(invalid("truncated entry digest"));
        }
        digest.copy_from_slice(&bytes[digest_start..digest_start + 32]);
        cursor += FOOTER_ENTRY_LEN;
        let offset = usize::try_from(offset).map_err(|_| invalid("entry offset overflows"))?;
        let length = usize::try_from(length).map_err(|_| invalid("entry length overflows"))?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| invalid("entry extent overflows"))?;
        // The chunk must lie in [header_end, footer_start): inside the file and never inside the directory itself.
        if offset < HEADER_LEN || end > footer_start || end < offset {
            return Err(invalid("a chunk lies outside the file's chunk region"));
        }
        if let Some(previous) = entries.last()
            && offset < previous.end()
        {
            return Err(invalid("chunks overlap or are out of order"));
        }
        entries.push(DirectoryEntry {
            chunk_type,
            offset,
            length,
            digest,
        });
    }
    // The footer-length word itself must sit exactly at the end.
    Ok(Footer { entries })
}

/// Whether the directory's chunk-type set is acceptable to a v1 reader: the critical types must be a subset of v1's,
/// and the ignorable types are free.
pub(crate) fn v1_accepts(entries: &[DirectoryEntry]) -> Result<(), CodecError> {
    for entry in entries {
        if !entry.is_ignorable() && !matches!(entry.chunk_type, CHUNK_NODE | CHUNK_STRG | CHUNK_NUMB | CHUNK_FACT) {
            return Err(CodecError::new(CodecFailureKind::UnsupportedRepresentation));
        }
    }
    Ok(())
}

/// Locates the critical core chunks in the directory.
///
/// v1 requires exactly one NODE, STRG, and NUMB chunk. FACT is critical when present (a duplicate refuses); a missing
/// FACT chunk is an empty fact table.
pub(crate) fn locate_core<'b>(entries: &[DirectoryEntry], bytes: &'b [u8]) -> Result<CoreChunks<'b>, CodecError> {
    let mut node = None;
    let mut strg = None;
    let mut numb = None;
    let mut fact = None;
    let mut prov = None;
    let mut sour = None;
    for entry in entries {
        let payload = slice(entry, bytes)?;
        match entry.chunk_type {
            CHUNK_NODE if node.is_none() => node = Some(payload),
            CHUNK_NODE => return Err(invalid("duplicate NODE chunk")),
            CHUNK_STRG if strg.is_none() => strg = Some(payload),
            CHUNK_STRG => return Err(invalid("duplicate STRG chunk")),
            CHUNK_NUMB if numb.is_none() => numb = Some(payload),
            CHUNK_NUMB => return Err(invalid("duplicate NUMB chunk")),
            CHUNK_FACT if fact.is_none() => fact = Some(payload),
            CHUNK_FACT => return Err(invalid("duplicate FACT chunk")),
            // Duplicate ignorable chunks are tolerated (the first wins) and unknown ignorable chunks are skipped for
            // free; unknown critical chunks were refused by `v1_accepts` already.
            CHUNK_PROV if prov.is_none() => prov = Some(payload),
            CHUNK_SOUR if sour.is_none() => sour = Some(payload),
            _ => {}
        }
    }
    Ok(CoreChunks {
        node: node.ok_or_else(|| invalid("the file carries no NODE chunk"))?,
        strg: strg.ok_or_else(|| invalid("the file carries no STRG chunk"))?,
        numb: numb.ok_or_else(|| invalid("the file carries no NUMB chunk"))?,
        fact: fact.unwrap_or_default(),
        prov,
        sour,
    })
}

/// The validated core chunks, borrowed from the source bytes.
pub(crate) struct CoreChunks<'a> {
    pub(crate) node: &'a [u8],
    pub(crate) strg: &'a [u8],
    pub(crate) numb: &'a [u8],
    pub(crate) fact: &'a [u8],
    pub(crate) prov: Option<&'a [u8]>,
    pub(crate) sour: Option<&'a [u8]>,
}

/// Slices one chunk's payload out of the file; the entry was already validated against the extent by `read_footer`, so
/// this cannot fail.
fn slice<'b>(entry: &DirectoryEntry, bytes: &'b [u8]) -> Result<&'b [u8], CodecError> {
    bytes
        .get(entry.offset..entry.end())
        .ok_or_else(|| invalid("a chunk extent lies outside the file"))
}

/// Node-table kinds (the `kind` byte of a node entry).
pub(crate) mod kinds {
    pub(crate) const NULL: u8 = 0;
    pub(crate) const BOOL: u8 = 1;
    pub(crate) const INTEGER: u8 = 2;
    pub(crate) const DECIMAL: u8 = 3;
    pub(crate) const FLOAT: u8 = 4;
    pub(crate) const STRING: u8 = 5;
    pub(crate) const BYTES: u8 = 6;
    pub(crate) const LOCAL_DATE: u8 = 7;
    pub(crate) const LOCAL_TIME: u8 = 8;
    pub(crate) const LOCAL_DATE_TIME: u8 = 9;
    pub(crate) const OFFSET_DATE_TIME: u8 = 10;
    pub(crate) const TAG: u8 = 11;
    pub(crate) const ARRAY: u8 = 12;
    pub(crate) const OBJECT: u8 = 13;
    /// An object member's text key (only valid in object key position; v1 keys are text-only — the model's object
    /// projection is string-keyed).
    pub(crate) const KEYTEXT: u8 = 14;
    pub(crate) const ENTRY_LEN: usize = 9;
}

/// A decoded node-table entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeEntry {
    pub(crate) kind: u8,
    pub(crate) subtree_size: u32,
    pub(crate) payload: u32,
}

/// Reads one node entry at `index`, validating the table extent.
pub(crate) fn read_node(table: &[u8], index: usize) -> Result<NodeEntry, CodecError> {
    let start = index
        .checked_mul(kinds::ENTRY_LEN)
        .ok_or_else(|| invalid("node index overflows"))?;
    let end = start
        .checked_add(kinds::ENTRY_LEN)
        .ok_or_else(|| invalid("node index overflows"))?;
    let entry = table
        .get(start..end)
        .ok_or_else(|| invalid("node index exceeds the node table"))?;
    Ok(NodeEntry {
        kind: entry[0],
        subtree_size: u32::from_le_bytes([entry[1], entry[2], entry[3], entry[4]]),
        payload: u32::from_le_bytes([entry[5], entry[6], entry[7], entry[8]]),
    })
}

/// The node table's entry count, validated against the chunk extent.
pub(crate) fn node_count(table: &[u8]) -> Result<usize, CodecError> {
    if !table.len().is_multiple_of(kinds::ENTRY_LEN) {
        return Err(invalid("the NODE chunk is not a whole number of entries"));
    }
    Ok(table.len() / kinds::ENTRY_LEN)
}

/// Reads one length-prefixed pool entry out of a pool chunk.
///
/// `offset` is the byte offset of the entry's length word. Returns the entry bytes and the offset of the NEXT entry. A
/// `[u8; 4]`-shaped u32 read.
pub(crate) fn pool_entry(pool: &[u8], offset: usize) -> Result<(&[u8], usize), CodecError> {
    let len = read_u32(pool, offset).ok_or_else(|| invalid("truncated pool entry length"))?;
    let len = usize::try_from(len).map_err(|_| invalid("pool entry length overflows"))?;
    let data_start = offset + 4;
    let data_end = data_start
        .checked_add(len)
        .ok_or_else(|| invalid("pool entry extent overflows"))?;
    let data = pool
        .get(data_start..data_end)
        .ok_or_else(|| invalid("pool entry exceeds the pool chunk"))?;
    Ok((data, data_end))
}

/// Reads the pool's entry count word.
pub(crate) fn pool_count(pool: &[u8]) -> Result<usize, CodecError> {
    let count = read_u64(pool, 0).ok_or_else(|| invalid("truncated pool count"))?;
    usize::try_from(count).map_err(|_| invalid("pool count overflows"))
}

/// Little-endian helpers over checked slices.
pub(crate) fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}

pub(crate) fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends one length-prefixed pool entry (no count word — the caller's pool builder tracks the entry count and writes
/// it at assembly).
pub(crate) fn push_pool_entry(pool: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CodecError> {
    let len = u32::try_from(bytes.len()).map_err(|_| invalid("pool entry too large"))?;
    push_u32(pool, len);
    pool.extend_from_slice(bytes);
    Ok(())
}

/// A clean `InvalidInput` failure naming the broken structural law.
///
/// The message is already written at every call site; wiring it into a source-less diagnostic is what makes a jqfb
/// structural rejection carry the same prose the text profiles do, rather than a bare kind.
pub(crate) fn invalid(message: &'static str) -> CodecError {
    let base = CodecError::new(CodecFailureKind::InvalidInput);
    let Some(diagnostic) =
        jqf_source::Diagnostic::try_new(Namespace::new("jqfb").code("invalid"), Severity::Error, message)
    else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

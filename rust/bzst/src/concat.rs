//! Verbatim concatenation of bzst files ([`concat`]).
//!
//! Unlike decompress-then-recompress, [`concat`] copies each input's compressed
//! data blocks byte-for-byte into a single output stream and rebuilds one index
//! over them. Nothing is decoded or re-encoded, so it is fast and preserves the
//! exact compressed bytes — and therefore baseline `zstd -d` decodability.
//!
//! Derived-format frames are copied through untouched: bzst treats them as opaque
//! and only regenerates the two structural frames it owns (the single header and
//! the single index).

use std::io::{Read, Write};

use crate::frame::{EncodedBlock, Frame, FrameReader, FrameWriter, Header};
use crate::index::IndexBuilder;
use crate::memory::default_alloc_limit;
use crate::{BzstError, BzstResult, Profiles};

/// Concatenates `inputs` into a single bzst stream written to `out`, copying
/// every data block verbatim (no decompression or re-compression) and rebuilding
/// one index over the combined blocks. Returns the underlying writer.
///
/// The output header is taken from the first input; all inputs must share the
/// bzst format version (enforced when their headers are parsed). Derived-format
/// frames — skippable frames under non-structural magics — are preserved in
/// place: `cat` treats them as opaque and never discards them. Only the two
/// structural frames bzst owns are regenerated: a single leading header (later
/// inputs' headers are dropped) and a single trailing index (each input's own
/// index is dropped and rebuilt for the combined file).
///
/// A derived format's *own* trailing index, if it has one, is copied through like
/// any other skippable frame; its offsets refer to the original file and so are
/// stale after concatenation. Reconciling derived (semantic) indices is a
/// derived-format concern, outside this generic operation (see the spec's
/// concatenation open issue). With no inputs, a valid empty baseline file (header
/// plus an empty index) results.
pub fn concat<R: Read, W: Write>(inputs: impl IntoIterator<Item = R>, out: W) -> BzstResult<W> {
    let mut fw = FrameWriter::new(out);
    let mut index = IndexBuilder::new();
    let mut header_written = false;

    for input in inputs {
        let mut fr = FrameReader::new(input, default_alloc_limit());
        match fr.next_frame()? {
            // The first input's header becomes the output header; later inputs'
            // headers are still validated (version, signature, checksum) on parse.
            Some(Frame::Header(header)) => {
                if !header_written {
                    fw.write_header(&header)?;
                    header_written = true;
                }
            }
            // Every bzst stream must open with a header frame.
            _ => return Err(BzstError::Malformed("input does not start with a header frame")),
        }
        loop {
            match fr.next_frame()? {
                None => break,
                Some(Frame::Block { header, data }) => {
                    let block = EncodedBlock { header, data: data.to_vec() };
                    let offset = fw.write_encoded_block(&block)?;
                    index.push(offset, block.on_disk_len(), block.header.uncompressed_size);
                }
                // Derived-format (and unknown) frames are opaque to bzst; copy
                // them through in place.
                Some(Frame::Skippable(frame)) => fw.write_skippable(frame.magic, frame.payload)?,
                // Each input's own bzst index is dropped; the combined index is
                // rebuilt from the blocks we copy.
                Some(Frame::Index(_)) => {}
                // A second header inside one stream is malformed.
                Some(Frame::Header(_)) => {
                    return Err(BzstError::Malformed("unexpected second header frame"))
                }
            }
        }
    }

    if !header_written {
        fw.write_header(&Header::new([0; 4], Profiles::BASELINE))?;
    }
    fw.write_index(&index.finish())?;
    fw.flush()?;
    Ok(fw.into_inner())
}

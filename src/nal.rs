//! Incremental Annex B (start-code delimited) NAL unit splitter.

/// HEVC NAL unit type used by Dolby Vision for the RPU (UNSPEC62).
pub const NAL_UNSPEC62_RPU: u8 = 62;
/// HEVC NAL unit type used by Dolby Vision to wrap enhancement layer NALs (UNSPEC63).
pub const NAL_UNSPEC63_EL: u8 = 63;

/// Returns the HEVC `nal_unit_type` of a NAL unit (including its 2 byte header).
pub fn hevc_nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3F)
}

/// Finds the next `00 00 01` start code at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < data.len() {
        // No start code can begin at i, i+1 or i+2 if this byte is > 1
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

fn trim_trailing_zeroes(nal: &[u8]) -> &[u8] {
    let end = nal.iter().rposition(|b| *b != 0).map_or(0, |p| p + 1);
    &nal[..end]
}

/// Splits a byte stream fed in arbitrary chunks into NAL units.
///
/// Any data before the first start code is discarded, which makes it
/// possible to start reading from the middle of a raw bitstream.
#[derive(Default)]
pub struct NalSplitter {
    buf: Vec<u8>,
    synced: bool,
    search_from: usize,
}

impl NalSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk of data. `on_nal` is called for every complete NAL unit
    /// (without start code) and returns `false` to stop processing.
    /// Returns `false` if processing was stopped.
    pub fn push(&mut self, data: &[u8], mut on_nal: impl FnMut(&[u8]) -> bool) -> bool {
        self.buf.extend_from_slice(data);

        if !self.synced {
            match find_start_code(&self.buf, 0) {
                Some(pos) => {
                    self.buf.drain(..pos);
                    self.synced = true;
                    self.search_from = 3;
                }
                None => {
                    // Keep the last two bytes, they may be the start of a start code
                    let keep_from = self.buf.len().saturating_sub(2);
                    self.buf.drain(..keep_from);
                    return true;
                }
            }
        }

        // buf always begins with a start code here
        let mut nal_start = 3;
        let mut search = self.search_from.max(3);

        while let Some(pos) = find_start_code(&self.buf, search) {
            let nal = trim_trailing_zeroes(&self.buf[nal_start..pos]);
            if !nal.is_empty() && !on_nal(nal) {
                return false;
            }
            nal_start = pos + 3;
            search = nal_start;
        }

        let consumed = nal_start - 3;
        let searched_until = self.buf.len().saturating_sub(2).max(nal_start);
        self.buf.drain(..consumed);
        self.search_from = searched_until - consumed;

        true
    }

    /// Flushes the last NAL unit at the end of the stream.
    pub fn finish(&mut self, mut on_nal: impl FnMut(&[u8]) -> bool) {
        if self.synced && self.buf.len() > 3 {
            let nal = trim_trailing_zeroes(&self.buf[3..]);
            if !nal.is_empty() {
                on_nal(nal);
            }
        }
        self.buf.clear();
        self.synced = false;
        self.search_from = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_all(data: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut splitter = NalSplitter::new();
        for c in data.chunks(chunk) {
            splitter.push(c, |n| {
                out.push(n.to_vec());
                true
            });
        }
        splitter.finish(|n| {
            out.push(n.to_vec());
            true
        });
        out
    }

    #[test]
    fn splits_regardless_of_chunk_size() {
        let stream = [
            0xFF, 0xEE, // garbage before the first start code
            0, 0, 0, 1, 0x40, 0x01, 0xAA, //
            0, 0, 1, 0x7C, 0x01, 0x19, 0x08, //
            0, 0, 0, 1, 0x7E, 0x01, 0x05, 0x00, 0x00,
        ];
        let expected = vec![
            vec![0x40, 0x01, 0xAA],
            vec![0x7C, 0x01, 0x19, 0x08],
            vec![0x7E, 0x01, 0x05],
        ];

        for chunk in 1..=stream.len() {
            assert_eq!(split_all(&stream, chunk), expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn nal_types() {
        assert_eq!(hevc_nal_type(&[0x7C, 0x01]), Some(NAL_UNSPEC62_RPU));
        assert_eq!(hevc_nal_type(&[0x7E, 0x01]), Some(NAL_UNSPEC63_EL));
        assert_eq!(hevc_nal_type(&[0x40, 0x01]), Some(32)); // VPS
    }
}

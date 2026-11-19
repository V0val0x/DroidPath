use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LineFramerError {
    #[error("Wire frame too large ({byte_count} bytes exceeds limit {limit})")]
    FrameTooLarge { byte_count: usize, limit: usize },
}

/// Stateful LF (`0x0A`) byte-stream framer for arbitrary TCP chunks.
/// Operates exclusively on raw UTF-8 bytes so multi-byte characters split across TCP packets
/// are never corrupted, and bounds memory strictly to `max_frame_bytes`.
#[derive(Debug, Clone)]
pub struct LineFramer {
    max_frame_bytes: usize,
    buffer: Vec<u8>,
    peak_buffer_bytes: usize,
}

impl LineFramer {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes: max_frame_bytes.max(1),
            buffer: Vec::new(),
            peak_buffer_bytes: 0,
        }
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn buffered_byte_count(&self) -> usize {
        self.buffer.len()
    }

    /// Appends an incoming TCP byte chunk and returns all complete newline-delimited frames.
    /// Empty lines (`\n` or `\r\n`) are skipped cleanly.
    pub fn append(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, LineFramerError> {
        if chunk.is_empty() {
            return Ok(Vec::new());
        }

        const LF: u8 = b'\n';
        const CR: u8 = b'\r';
        let max_bytes = self.max_frame_bytes;
        let mut frames = Vec::new();
        let mut segment_start = 0usize;

        for (i, &byte) in chunk.iter().enumerate() {
            if byte == LF {
                let mut segment_end = i;
                if segment_end > segment_start && chunk[segment_end - 1] == CR {
                    segment_end -= 1;
                }
                let slice = &chunk[segment_start..segment_end];

                if self.buffer.is_empty() {
                    if slice.len() > max_bytes {
                        self.reset();
                        return Err(LineFramerError::FrameTooLarge {
                            byte_count: slice.len(),
                            limit: max_bytes,
                        });
                    }
                    if !slice.is_empty() {
                        frames.push(slice.to_vec());
                    }
                } else {
                    let combined_count = self.buffer.len() + slice.len();
                    if combined_count > max_bytes {
                        self.reset();
                        return Err(LineFramerError::FrameTooLarge {
                            byte_count: combined_count,
                            limit: max_bytes,
                        });
                    }
                    if !slice.is_empty() {
                        self.buffer.extend_from_slice(slice);
                    }
                    if self.buffer.last() == Some(&CR) {
                        self.buffer.pop();
                    }
                    if !self.buffer.is_empty() {
                        frames.push(self.buffer.clone());
                    }
                    if self.peak_buffer_bytes > 256 * 1024 {
                        self.buffer = Vec::new();
                        self.peak_buffer_bytes = 0;
                    } else {
                        self.buffer.clear();
                    }
                }
                segment_start = i + 1;
            }
        }

        if segment_start < chunk.len() {
            let tail = &chunk[segment_start..];
            let next_buffered_count = self.buffer.len() + tail.len();
            if next_buffered_count > max_bytes {
                self.reset();
                return Err(LineFramerError::FrameTooLarge {
                    byte_count: next_buffered_count,
                    limit: max_bytes,
                });
            }
            self.buffer.extend_from_slice(tail);
            if self.buffer.len() > self.peak_buffer_bytes {
                self.peak_buffer_bytes = self.buffer.len();
            }
        }

        Ok(frames)
    }

    pub fn reset(&mut self) {
        self.buffer = Vec::new();
        self.peak_buffer_bytes = 0;
    }
}

impl Default for LineFramer {
    fn default() -> Self {
        Self::new(8 * 1024 * 1024)
    }
}

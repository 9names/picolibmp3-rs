#![deny(unsafe_op_in_unsafe_fn)]

use crate::contig_buffer;
use crate::mp3::{DecodeErr, MP3FrameInfo, Mp3};

pub struct EasyMode {
    mp3: Mp3,
    buffer: contig_buffer::Buffer,
    sync: bool,
    have_decoded: bool,
    parsed_id3: bool,
    bytes_to_skip: usize,
    frame_info: Option<MP3FrameInfo>,
}

impl EasyMode {
    /// Construct a new "easy mode" MP3 decoder
    pub const fn new() -> Self {
        EasyMode {
            mp3: Mp3::new(),
            buffer: contig_buffer::Buffer::new(),
            sync: false,
            have_decoded: false,
            parsed_id3: false,
            bytes_to_skip: 0,
            frame_info: None,
        }
    }

    /// Add MP3 data to the EasyMode internal MP3 stream buffer
    /// This function will also attempt to find the start of stream
    pub fn add_data(&mut self, data: &[u8]) -> usize {
        let bytes_added = self.buffer.load_slice(data);
        let _ = self.find_next_sync_word();
        bytes_added
    }

    pub fn find_next_sync_word(&mut self) -> bool {
        if !self.sync {
            let start = Mp3::find_sync_word(self.buffer.borrow_slice());
            if start >= 0 {
                self.buffer.increment_start(start as usize);
                self.sync = true;
                // Also try to get frame info for next frame
                let f = self.mp3.get_next_frame_info(self.buffer.borrow_slice());
                if let Ok(frame) = f {
                    self.frame_info = Some(frame);
                    self.have_decoded = true;
                }
            } else {
                // Could not sync with any of the data in the buffer, so most of the data is useless.
                // we could have 3 bytes of sync word, so keep the last 3 bytes
                self.buffer.increment_start(self.buffer.used() - 3);
            }
        }
        self.sync
    }

    /// Add MP3 data to the EasyMode internal MP3 stream buffer
    pub fn add_data_no_sync(&mut self, data: &[u8]) -> usize {
        self.buffer.load_slice(data)
    }

    /// How much data is free in the EasyMode internal MP3 stream buffer
    pub fn buffer_free(&self) -> usize {
        self.buffer.available()
    }

    /// How much MP3 data is in the EasyMode internal MP3 stream buffer
    pub fn buffer_used(&self) -> usize {
        self.buffer.used()
    }

    /// Skip over data in the buffer without decoding it
    pub fn buffer_skip(&mut self, count: usize) -> usize {
        let to_remove = core::cmp::min(self.buffer.used(), count);
        self.buffer.increment_start(to_remove);
        to_remove
    }

    /// Skip over ID3 and anything else at the start of an MP3 stream.
    /// Returns true when we've got a valid MP3 frame
    pub fn mp3_decode_ready(&mut self) -> bool {
        if self.buffer_used() == 0 {
            false
        } else {
            if !self.parsed_id3 {
                self.parsed_id3 = true;
                let id3 = self.find_id3v2();
                self.bytes_to_skip = if let Some(id3) = id3 {
                    // start of header + size of header + length
                    id3.0 + 10 + id3.4
                } else {
                    0
                };
            };
            if self.bytes_to_skip > 0 {
                let bytes_to_skip = core::cmp::min(self.buffer_used(), self.bytes_to_skip);
                self.buffer_skip(bytes_to_skip);
                self.bytes_to_skip -= bytes_to_skip;
            } else {
                let _ = self.find_next_sync_word();
            }

            self.parsed_id3 && self.bytes_to_skip == 0 && self.sync
        }
    }

    /// Decode the next MP3 audio frame after checking that the output buffer is large enough
    pub fn decode(&mut self, output_audio: &mut [i16]) -> Result<usize, EasyModeErr> {
        let buffered_data_len = self.buffer.used() as i32;
        let oldlen = buffered_data_len as usize;
        let next_frame = self.mp3.get_next_frame_info(self.buffer.borrow_slice())?;
        let samples = next_frame.outputSamps as usize;
        if output_audio.len() < samples {
            // Don't decode if there isn't enough space in the buffer
            Err(EasyModeErr::AudioBufferTooSmall)
        } else {
            match self
                .mp3
                .decode(self.buffer.borrow_slice(), buffered_data_len, output_audio)
            {
                Ok(newlen) => {
                    self.have_decoded = true;
                    let consumed = oldlen - newlen as usize;
                    self.buffer.increment_start(consumed);
                    self.frame_info = Some(next_frame);
                    Ok(samples)
                }
                Err(e) => Err(e.into()),
            }
        }
    }

    /// Decode the next MP3 audio frame assuming that the output buffer is large enough.
    ///
    /// # Safety
    ///
    /// Ensure output buffer is larger than your MP3 frame or this will totally ruin your day
    pub unsafe fn decode_unchecked(
        &mut self,
        output_audio: &mut [i16],
    ) -> Result<usize, EasyModeErr> {
        let buffered_data_len = self.buffer.used() as i32;
        let oldlen = buffered_data_len;
        match self
            .mp3
            .decode(self.buffer.borrow_slice(), buffered_data_len, output_audio)
        {
            Ok(newlen) => {
                self.frame_info = Some(self.mp3.get_last_frame_info());
                // we just set this so the unwrap should never fail
                let output_samps = unsafe {self.frame_info.unwrap_unchecked().outputSamps};
                let consumed = oldlen as usize - newlen as usize;
                self.buffer.increment_start(consumed);
                self.have_decoded = true;
                Ok(output_samps as usize)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Get MP3 metadata from the last MP3 frame decoded
    pub fn mp3_info(&mut self) -> Result<MP3FrameInfo, EasyModeErr> {
        if let Some(frameinfo) = self.frame_info {
            Ok(frameinfo)
        } else {
            let frame = self.mp3.get_next_frame_info(self.buffer.borrow_slice())?;
            Ok(frame)
        }
    }

    // from https://mutagen-specs.readthedocs.io/en/latest/id3/id3v2.4.0-structure.html
    // ID3 tag format is as follows
    // $49 44 33 yy yy xx zz zz zz zz
    // yy yy is the version, xx is flags, zz zz zz zz is the ID3v2 tag size.
    //
    /// Find and decode ID3v2 header info
    pub fn find_id3v2(&mut self) -> Option<(usize, u8, u8, u8, usize)> {
        let window = self.buffer.borrow_slice().windows(10);
        for (offset, slice) in window.enumerate() {
            if let [b'I', b'D', b'3', major, minor, flags, s1, s2, s3, s4] = slice {
                // The ID3v2 tag size is stored as a 32 bit synchsafe integer, making a total of 28 effective bits (representing up to 256MB).
                // a syncsafe integer is a 7bit integer where the top bit is always zero.
                if (s1 | s2 | s3 | s4) & 0b1000_0000 != 0b1000_0000 {
                    let (s1, s2, s3, s4) = (*s1 as usize, *s2 as usize, *s3 as usize, *s4 as usize);
                    let size = s4 | s3 << 7 | s2 << 14 | s1 << 21;
                    return Some((offset, *major, *minor, *flags, size));
                }
            }
        }
        None
    }
}

/// Errors that occur when calling the decode function
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum EasyModeErr {
    Okay,
    InDataUnderflow,
    MaindataUnderfow,
    FreeBitrateSync,
    OutOfMemory,
    NullPointer,
    InvalidFrameheader,
    InvalidSideinfo,
    InvalidScalefact,
    InvalidHuffcodes,
    InvalidDequantize,
    InvalidImdct,
    InvalidSubband,
    Unknown,
    InvalidError,
    AudioBufferTooSmall,
}

impl From<DecodeErr> for EasyModeErr {
    fn from(value: DecodeErr) -> Self {
        match value {
            DecodeErr::Okay => EasyModeErr::Okay,
            DecodeErr::InDataUnderflow => EasyModeErr::InDataUnderflow,
            DecodeErr::MaindataUnderfow => EasyModeErr::MaindataUnderfow,
            DecodeErr::FreeBitrateSync => EasyModeErr::FreeBitrateSync,
            DecodeErr::OutOfMemory => EasyModeErr::OutOfMemory,
            DecodeErr::NullPointer => EasyModeErr::NullPointer,
            DecodeErr::InvalidFrameheader => EasyModeErr::InvalidFrameheader,
            DecodeErr::InvalidSideinfo => EasyModeErr::InvalidSideinfo,
            DecodeErr::InvalidScalefact => EasyModeErr::InvalidScalefact,
            DecodeErr::InvalidHuffcodes => EasyModeErr::InvalidHuffcodes,
            DecodeErr::InvalidDequantize => EasyModeErr::InvalidDequantize,
            DecodeErr::InvalidImdct => EasyModeErr::InvalidImdct,
            DecodeErr::InvalidSubband => EasyModeErr::InvalidSubband,
            DecodeErr::Unknown => EasyModeErr::Unknown,
            DecodeErr::InvalidError => EasyModeErr::InvalidError,
        }
    }
}

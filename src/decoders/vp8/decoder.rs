// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::anyhow;
use anyhow::Result;

use crate::decoders::vp8::backends::StatelessDecoderBackend;
use crate::decoders::vp8::parser::Frame;
use crate::decoders::vp8::parser::Header;
use crate::decoders::vp8::parser::Parser;
use crate::decoders::BlockingMode;
use crate::decoders::DecodedHandle;
use crate::decoders::Result as VideoDecoderResult;
use crate::decoders::VideoDecoder;
use crate::Resolution;

/// Represents where we are in the negotiation status. We assume ownership of
/// the incoming buffers in this special case so that clients do not have to do
/// the bookkeeping themselves.
enum NegotiationStatus {
    /// Still waiting for a key frame.
    NonNegotiated,

    /// Saw a key frame. Negotiation is possible until the next call to decode()
    Possible {
        key_frame: (u64, Box<Header>, Vec<u8>, Box<Parser>),
    },

    /// Negotiated. Locks in the format until a new key frame is seen if that
    /// new key frame changes the stream parameters.
    Negotiated,
}

impl Default for NegotiationStatus {
    fn default() -> Self {
        Self::NonNegotiated
    }
}

pub struct Decoder<T: DecodedHandle> {
    /// A parser to extract bitstream data and build frame data in turn
    parser: Parser,

    /// Whether the decoder should block on decode operations.
    blocking_mode: BlockingMode,

    /// The backend used for hardware acceleration.
    backend: Box<dyn StatelessDecoderBackend<Handle = T>>,

    /// Keeps track of whether the decoded format has been negotiated with the
    /// backend.
    negotiation_status: NegotiationStatus,

    /// The current resolution
    coded_resolution: Resolution,

    /// A queue with the pictures that are ready to be sent to the client.
    ready_queue: Vec<T>,

    /// A monotonically increasing counter used to tag pictures in display
    /// order
    current_display_order: u64,

    /// The picture used as the last reference picture.
    last_picture: Option<T>,
    /// The picture used as the golden reference picture.
    golden_ref_picture: Option<T>,
    /// The picture used as the alternate reference picture.
    alt_ref_picture: Option<T>,
}

impl<T: DecodedHandle + Clone + 'static> Decoder<T> {
    /// Create a new codec backend for VP8.
    #[cfg(any(feature = "vaapi", test))]
    pub(crate) fn new(
        backend: Box<dyn StatelessDecoderBackend<Handle = T>>,
        blocking_mode: BlockingMode,
    ) -> Result<Self> {
        Ok(Self {
            backend,
            blocking_mode,
            // wait_keyframe: true,
            parser: Default::default(),
            negotiation_status: Default::default(),
            last_picture: Default::default(),
            golden_ref_picture: Default::default(),
            alt_ref_picture: Default::default(),
            coded_resolution: Default::default(),
            ready_queue: Default::default(),
            current_display_order: Default::default(),
        })
    }

    /// Replace a reference frame with `handle`.
    fn replace_reference(reference: &mut Option<T>, handle: &T) {
        *reference = Some(handle.clone());
    }

    pub(crate) fn update_references(
        header: &Header,
        decoded_handle: &T,
        last_picture: &mut Option<T>,
        golden_ref_picture: &mut Option<T>,
        alt_ref_picture: &mut Option<T>,
    ) -> Result<()> {
        if header.key_frame() {
            Decoder::replace_reference(last_picture, decoded_handle);
            Decoder::replace_reference(golden_ref_picture, decoded_handle);
            Decoder::replace_reference(alt_ref_picture, decoded_handle);
        } else {
            if header.refresh_alternate_frame() {
                Decoder::replace_reference(alt_ref_picture, decoded_handle);
            } else {
                match header.copy_buffer_to_alternate() {
                    0 => { /* do nothing */ }

                    1 => {
                        if let Some(last_picture) = last_picture {
                            Decoder::replace_reference(alt_ref_picture, last_picture);
                        }
                    }

                    2 => {
                        if let Some(golden_ref) = golden_ref_picture {
                            Decoder::replace_reference(alt_ref_picture, golden_ref);
                        }
                    }

                    other => panic!("Invalid value: {}", other),
                }
            }

            if header.refresh_golden_frame() {
                Decoder::replace_reference(golden_ref_picture, decoded_handle);
            } else {
                match header.copy_buffer_to_golden() {
                    0 => { /* do nothing */ }

                    1 => {
                        if let Some(last_picture) = last_picture {
                            Decoder::replace_reference(golden_ref_picture, last_picture);
                        }
                    }

                    2 => {
                        if let Some(alt_ref) = alt_ref_picture {
                            Decoder::replace_reference(golden_ref_picture, alt_ref);
                        }
                    }

                    other => panic!("Invalid value: {}", other),
                }
            }

            if header.refresh_last() {
                Decoder::replace_reference(last_picture, decoded_handle);
            }
        }

        Ok(())
    }

    fn block_on_one(&mut self) -> Result<()> {
        if let Some(handle) = self.ready_queue.first() {
            return self.backend.block_on_handle(handle).map_err(|e| anyhow!(e));
        }

        Ok(())
    }

    /// Returns the ready handles.
    fn get_ready_frames(&mut self) -> Vec<T> {
        // Count all ready handles.
        let num_ready = self
            .ready_queue
            .iter()
            .take_while(|&handle| self.backend.handle_is_ready(handle))
            .count();

        let retain = self.ready_queue.split_off(num_ready);
        // `split_off` works the opposite way of what we would like, leaving [0..num_ready) in
        // place, so we need to swap `retain` with `ready_queue`.
        let ready = std::mem::take(&mut self.ready_queue);
        self.ready_queue = retain;

        ready
    }

    /// Handle a single frame.
    fn handle_frame(
        &mut self,
        frame: Frame<&[u8]>,
        timestamp: u64,
        queued_parser_state: Option<Parser>,
    ) -> Result<()> {
        let parser = match &queued_parser_state {
            Some(parser) => parser,
            None => &self.parser,
        };

        let block = if matches!(self.blocking_mode, BlockingMode::Blocking)
            || matches!(self.negotiation_status, NegotiationStatus::Possible { .. })
        {
            BlockingMode::Blocking
        } else {
            BlockingMode::NonBlocking
        };

        let show_frame = frame.header.show_frame();

        let mut decoded_handle = self
            .backend
            .submit_picture(
                &frame.header,
                self.last_picture.as_ref(),
                self.golden_ref_picture.as_ref(),
                self.alt_ref_picture.as_ref(),
                frame.bitstream,
                parser.segmentation(),
                parser.mb_lf_adjust(),
                timestamp,
                block,
            )
            .map_err(|e| anyhow!(e))?;

        // Do DPB management
        Self::update_references(
            &frame.header,
            &decoded_handle,
            &mut self.last_picture,
            &mut self.golden_ref_picture,
            &mut self.alt_ref_picture,
        )?;

        if show_frame {
            let order = self.current_display_order;

            decoded_handle.set_display_order(order);
            self.current_display_order += 1;
            self.ready_queue.push(decoded_handle);
        }

        Ok(())
    }

    fn negotiation_possible(&self, frame: &Frame<impl AsRef<[u8]>>) -> bool {
        let coded_resolution = self.coded_resolution;
        let hdr = &frame.header;
        let width = u32::from(hdr.width());
        let height = u32::from(hdr.height());

        width != coded_resolution.width || height != coded_resolution.height
    }
}

impl<T: DecodedHandle + Clone + 'static> VideoDecoder for Decoder<T> {
    fn decode(
        &mut self,
        timestamp: u64,
        bitstream: &[u8],
    ) -> VideoDecoderResult<Vec<Box<dyn DecodedHandle>>> {
        let frame = self.parser.parse_frame(bitstream).map_err(|e| anyhow!(e))?;

        if frame.header.key_frame() {
            if self.negotiation_possible(&frame)
                && matches!(self.negotiation_status, NegotiationStatus::Negotiated)
            {
                self.negotiation_status = NegotiationStatus::NonNegotiated;
            }
        }

        match &mut self.negotiation_status {
            NegotiationStatus::NonNegotiated => {
                if frame.header.key_frame() {
                    self.backend.poll(BlockingMode::Blocking)?;

                    self.backend.new_sequence(&frame.header)?;

                    self.coded_resolution = Resolution {
                        width: u32::from(frame.header.width()),
                        height: u32::from(frame.header.height()),
                    };

                    self.negotiation_status = NegotiationStatus::Possible {
                        key_frame: (
                            timestamp,
                            Box::new(frame.header),
                            Vec::from(frame.bitstream),
                            Box::new(self.parser.clone()),
                        ),
                    }
                }

                return Ok(vec![]);
            }

            NegotiationStatus::Possible { key_frame } => {
                let (timestamp, header, bitstream, parser) = key_frame.clone();
                let key_frame = Frame {
                    bitstream: bitstream.as_ref(),
                    header: *header,
                };

                self.handle_frame(key_frame, timestamp, Some(*parser))?;

                self.negotiation_status = NegotiationStatus::Negotiated;
            }

            NegotiationStatus::Negotiated => (),
        };

        self.handle_frame(frame, timestamp, None)?;

        if self.backend.num_resources_left() == 0 {
            self.block_on_one()?;
        }

        self.backend.poll(self.blocking_mode)?;

        let ready_frames = self.get_ready_frames();

        Ok(ready_frames
            .into_iter()
            .map(|h| Box::new(h) as Box<dyn DecodedHandle>)
            .collect())
    }

    fn flush(&mut self) -> crate::decoders::Result<Vec<Box<dyn DecodedHandle>>> {
        // Decode whatever is pending using the default format. Mainly covers
        // the rare case where only one buffer is sent.
        if let NegotiationStatus::Possible { key_frame } = &self.negotiation_status {
            let (timestamp, header, bitstream, parser) = key_frame;

            let bitstream = bitstream.clone();
            let header = header.as_ref().clone();

            let key_frame = Frame {
                bitstream: bitstream.as_ref(),
                header,
            };
            let timestamp = *timestamp;
            let parser = *parser.clone();

            self.handle_frame(key_frame, timestamp, Some(parser))?;
        }

        self.backend.poll(BlockingMode::Blocking)?;

        let pics = self.get_ready_frames();

        Ok(pics
            .into_iter()
            .map(|h| Box::new(h) as Box<dyn DecodedHandle>)
            .collect())
    }

    fn negotiation_possible(&self) -> bool {
        matches!(self.negotiation_status, NegotiationStatus::Possible { .. })
    }

    fn num_resources_left(&self) -> Option<usize> {
        if matches!(self.negotiation_status, NegotiationStatus::NonNegotiated) {
            return None;
        }

        let left_in_the_backend = self.backend.num_resources_left();

        if let NegotiationStatus::Possible { .. } = &self.negotiation_status {
            Some(left_in_the_backend - 1)
        } else {
            Some(left_in_the_backend)
        }
    }

    fn num_resources_total(&self) -> usize {
        self.backend.num_resources_total()
    }

    fn coded_resolution(&self) -> Option<Resolution> {
        self.backend.coded_resolution()
    }

    fn poll(
        &mut self,
        blocking_mode: BlockingMode,
    ) -> VideoDecoderResult<Vec<Box<dyn DecodedHandle>>> {
        let handles = self.backend.poll(blocking_mode)?;

        Ok(handles
            .into_iter()
            .map(|h| Box::new(h) as Box<dyn DecodedHandle>)
            .collect())
    }
}
#[cfg(test)]
pub mod tests {
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Seek;

    use bytes::Buf;

    use crate::decoders::tests::test_decode_stream;
    use crate::decoders::tests::TestStream;
    use crate::decoders::vp8::decoder::Decoder;
    use crate::decoders::BlockingMode;
    use crate::decoders::DecodedHandle;
    use crate::decoders::VideoDecoder;

    /// Read and return the data from the next IVF packet. Returns `None` if there is no more data
    /// to read.
    fn read_ivf_packet(cursor: &mut Cursor<&[u8]>) -> Option<Box<[u8]>> {
        if !cursor.has_remaining() {
            return None;
        }

        let len = cursor.get_u32_le();
        // Skip PTS.
        let _ = cursor.get_u64_le();

        let mut buf = vec![0u8; len as usize];
        cursor.read_exact(&mut buf).unwrap();

        Some(buf.into_boxed_slice())
    }

    pub fn vp8_decoding_loop<D>(
        decoder: &mut D,
        test_stream: &[u8],
        on_new_frame: &mut dyn FnMut(Box<dyn DecodedHandle>),
    ) where
        D: VideoDecoder,
    {
        let mut cursor = Cursor::new(test_stream);
        let mut frame_num = 0;

        // Skip the IVH header entirely.
        cursor.seek(std::io::SeekFrom::Start(32)).unwrap();

        while let Some(packet) = read_ivf_packet(&mut cursor) {
            for frame in decoder.decode(frame_num, packet.as_ref()).unwrap() {
                on_new_frame(frame);
                frame_num += 1;
            }
        }

        for frame in decoder.flush().unwrap() {
            on_new_frame(frame);
            frame_num += 1;
        }
    }

    /// Run `test` using the dummy decoder, in both blocking and non-blocking modes.
    fn test_decoder_dummy(test: &TestStream, blocking_mode: BlockingMode) {
        let decoder = Decoder::new_dummy(blocking_mode).unwrap();

        test_decode_stream(vp8_decoding_loop, decoder, test, false, false);
    }

    /// Same as Chromium's test-25fps.vp8
    pub const DECODE_TEST_25FPS: TestStream = TestStream {
        stream: include_bytes!("test_data/test-25fps.vp8"),
        crcs: include_str!("test_data/test-25fps.vp8.crc"),
    };

    #[test]
    fn test_25fps_block() {
        test_decoder_dummy(&DECODE_TEST_25FPS, BlockingMode::Blocking);
    }

    #[test]
    fn test_25fps_nonblock() {
        test_decoder_dummy(&DECODE_TEST_25FPS, BlockingMode::NonBlocking);
    }
}

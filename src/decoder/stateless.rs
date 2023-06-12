// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod h264;
pub mod h265;
pub mod vp8;
pub mod vp9;

use thiserror::Error;

use crate::decoder::DecodedHandle;
use crate::decoder::DecoderEvent;
use crate::decoder::DecoderFormatNegotiator;
use crate::DecodedFormat;
use crate::Resolution;

#[derive(Error, Debug)]
pub enum StatelessBackendError {
    #[error("not enough resources to proceed with the operation now")]
    OutOfResources,
    #[error("this resource is not ready")]
    ResourceNotReady,
    #[error("this format is not supported")]
    UnsupportedFormat,
    #[error("negotiation failed")]
    NegotiationFailed(anyhow::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type StatelessBackendResult<T> = std::result::Result<T, StatelessBackendError>;

/// Decoder implementations can use this struct to represent their decoding state.
#[derive(Default)]
enum DecodingState<T> {
    /// Decoder will ignore all input until format and resolution information passes by.
    #[default]
    AwaitingStreamInfo,
    /// Decoder is stopped until the client has confirmed the output format.
    AwaitingFormat(T),
    /// Decoder is currently decoding input.
    Decoding,
}

#[derive(Debug, Error)]
/// Error possibly returned by the `decode` method..
pub enum DecodeError {
    #[error("cannot accept more input until pending events are processed")]
    CheckEvents,
    #[error("decoder error: {0}")]
    DecoderError(#[from] anyhow::Error),
    #[error("backend error: {0}")]
    BackendError(#[from] StatelessBackendError),
}

mod private {
    use super::*;

    /// Private trait for methods we need to expose for crate types (e.g.
    /// `DecoderFormatNegotiator`s), but don't want to be directly used by the client.
    pub(crate) trait StatelessVideoDecoder {
        /// Try to apply `format` to output frames. If successful, all frames emitted after the
        /// call will be in the new format.
        fn try_format(&mut self, format: DecodedFormat) -> anyhow::Result<()>;
    }
}

/// Common trait shared by all stateless video decoder backends, providing codec-independent
/// methods.
pub(crate) trait StatelessDecoderBackend<FormatInfo> {
    /// The type that the backend returns as a result of a decode operation.
    /// This will usually be some backend-specific type with a resource and a
    /// resource pool so that said buffer can be reused for another decode
    /// operation when it goes out of scope.
    type Handle: DecodedHandle + Clone;

    /// Returns the current coded resolution of the bitstream being processed.
    /// This may be None if we have not read the stream parameters yet.
    fn coded_resolution(&self) -> Option<Resolution>;

    /// Returns the current display resolution of the bitstream being processed.
    /// This may be None if we have not read the stream parameters yet.
    fn display_resolution(&self) -> Option<Resolution>;

    /// Gets the number of output resources allocated by the backend.
    fn num_resources_total(&self) -> usize;

    /// Gets the number of output resources left in the backend.
    fn num_resources_left(&self) -> usize;

    /// Gets the chosen format. This is set to a default after the decoder reads
    /// enough stream metadata from the bitstream. Some buffers need to be
    /// processed first before the default format can be set.
    fn format(&self) -> Option<DecodedFormat>;

    /// Try altering the decoded format.
    fn try_format(&mut self, format_info: &FormatInfo, format: DecodedFormat)
        -> anyhow::Result<()>;
}

/// Helper to implement `DecoderFormatNegotiator` for stateless decoders.
struct StatelessDecoderFormatNegotiator<'a, D, H, F>
where
    D: StatelessVideoDecoder,
    F: Fn(&mut D, &H),
{
    decoder: &'a mut D,
    format_hint: H,
    apply_format: F,
}

impl<'a, D, H, F> StatelessDecoderFormatNegotiator<'a, D, H, F>
where
    D: StatelessVideoDecoder,
    F: Fn(&mut D, &H),
{
    /// Creates a new format negotiator.
    ///
    /// `decoder` is the decoder negotiation is done for. The decoder is exclusively borrowed as
    /// long as this object exists.
    ///
    /// `format_hint` is a codec-specific structure describing the properties of the format.
    ///
    /// `apply_format` is a closure called when the object is dropped, and is responsible for
    /// applying the format and allowing decoding to resume.
    fn new(decoder: &'a mut D, format_hint: H, apply_format: F) -> Self {
        Self {
            decoder,
            format_hint,
            apply_format,
        }
    }
}

impl<'a, D, H, F> DecoderFormatNegotiator<'a> for StatelessDecoderFormatNegotiator<'a, D, H, F>
where
    D: StatelessVideoDecoder + private::StatelessVideoDecoder,
    F: Fn(&mut D, &H),
{
    fn num_resources_total(&self) -> usize {
        self.decoder.num_resources_total()
    }

    fn coded_resolution(&self) -> Resolution {
        self.decoder.coded_resolution().unwrap()
    }

    /// Returns the current output format, if one is currently set.
    fn format(&self) -> Option<DecodedFormat> {
        self.decoder.format()
    }

    /// Try to apply `format` to output frames. If successful, all frames emitted after the
    /// call will be in the new format.
    fn try_format(&mut self, format: DecodedFormat) -> anyhow::Result<()> {
        self.decoder.try_format(format)
    }
}

impl<'a, D, H, F> Drop for StatelessDecoderFormatNegotiator<'a, D, H, F>
where
    D: StatelessVideoDecoder,
    F: Fn(&mut D, &H),
{
    fn drop(&mut self) {
        (self.apply_format)(self.decoder, &self.format_hint)
    }
}

/// Stateless video decoder interface.
///
/// A stateless decoder differs from a stateful one in that its input and output queues are not
/// operating independently: a new decode unit can only be processed if there is already an output
/// resource available to receive its decoded content.
///
/// Therefore `decode` can refuse work if there is no output resource available at the time of
/// calling, in which case the caller is responsible for calling `decode` again with the same
/// parameters after processing at least one pending output frame and returning it to the decoder.
pub trait StatelessVideoDecoder {
    /// Try to decode the `bitstream` represented by `timestamp`.
    ///
    /// This method will return `DecodeError::CheckEvents` if processing cannot take place until
    /// pending events are handled. This could either be because a change of output format has
    /// been detected that the client should acknowledge, or because there are no available output
    /// resources and dequeueing and returning pending frames will fix that. After the cause has
    /// been addressed, the client is responsible for calling this method again with the same data.
    fn decode(&mut self, timestamp: u64, bitstream: &[u8]) -> std::result::Result<(), DecodeError>;

    /// Flush the decoder i.e. finish processing all pending decode requests and make sure the
    /// resulting frames are ready to be retrieved via `next_event`.
    fn flush(&mut self);

    /// Gets the number of output resources left in the backend.
    fn num_resources_left(&self) -> usize;

    /// Gets the number of output resources allocated by the backend.
    fn num_resources_total(&self) -> usize;

    /// Returns the current coded resolution of the bitstream being processed.
    /// This may be None if we have not read the stream parameters yet.
    fn coded_resolution(&self) -> Option<Resolution>;

    /// Returns the next event, if there is any pending.
    fn next_event(&mut self) -> Option<DecoderEvent>;

    /// Returns the current output format, if one is currently set.
    fn format(&self) -> Option<DecodedFormat>;
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::decoder::stateless::StatelessVideoDecoder;
    use crate::decoder::DecodedHandle;

    /// Stream that can be used in tests, along with the CRC32 of all of its frames.
    pub struct TestStream {
        /// Bytestream to decode.
        pub stream: &'static [u8],
        /// Expected CRC for each frame, one per line.
        pub crcs: &'static str,
    }

    /// Run the codec-specific `decoding_loop` on a `decoder` with a given `test`, linearly
    /// decoding the stream until its end.
    ///
    /// If `check_crcs` is `true`, then the expected CRCs of the decoded images are compared
    /// against the existing result. We may want to set this to false when using a decoder backend
    /// that does not produce actual frames.
    ///
    /// `dump_yuv` will dump all the decoded frames into `/tmp/framexxx.yuv`. Set this to true in
    /// order to debug the output of the test.
    pub fn test_decode_stream<D, L>(
        decoding_loop: L,
        mut decoder: D,
        test: &TestStream,
        check_crcs: bool,
        dump_yuv: bool,
    ) where
        D: StatelessVideoDecoder,
        L: Fn(&mut D, &[u8], &mut dyn FnMut(Box<dyn DecodedHandle>)),
    {
        let mut crcs = test.crcs.lines().enumerate();

        decoding_loop(&mut decoder, test.stream, &mut |handle| {
            let (frame_num, expected_crc) = crcs.next().expect("decoded more frames than expected");

            if check_crcs || dump_yuv {
                let mut picture = handle.dyn_picture_mut();
                let mut backend_handle = picture.dyn_mappable_handle_mut();

                let buffer_size = backend_handle.image_size();
                let mut nv12 = vec![0; buffer_size];

                backend_handle.read(&mut nv12).unwrap();

                if dump_yuv {
                    std::fs::write(format!("/tmp/frame{:03}.yuv", frame_num), &nv12).unwrap();
                }

                if check_crcs {
                    let frame_crc = format!("{:08x}", crc32fast::hash(&nv12));
                    assert_eq!(frame_crc, expected_crc, "at frame {}", frame_num);
                }
            }
        });

        assert_eq!(crcs.next(), None, "decoded less frames than expected");
    }
}
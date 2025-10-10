use bytes::{Buf as _, BufMut as _};

/// Decodes the top level header, extracts the version, and re-encodes the expected inner header to the returned vec.
pub fn decode_rlp_version(buf: &mut &[u8]) -> alloy_rlp::Result<(u8, Vec<u8>)> {
    if buf.is_empty() {
        return Err(alloy_rlp::Error::InputTooShort);
    }
    // decode outer header
    let alloy_rlp::Header {
        list,
        payload_length,
    } = alloy_rlp::Header::decode(buf)?;

    // validate outer header
    if !list {
        return Err(alloy_rlp::Error::UnexpectedString);
    }
    let started_len = buf.len();
    if started_len < payload_length {
        return Err(alloy_rlp::Error::InputTooShort);
    }
    if buf.is_empty() {
        return Err(alloy_rlp::Error::InputTooShort);
    }
    // decode version
    let version: u8 = alloy_rlp::Decodable::decode(buf)?;

    let mut tmp = Vec::new();
    // encode the inner header
    // validation will happen in the decode methods for the txs
    let header = alloy_rlp::Header {
        list,
        payload_length: payload_length - 1,
    };

    header.encode(&mut tmp);
    // TODO: figure out how to remove this copy - we can't overwrite the now-defunct external header as the slice isn't &mut &mut [u8] (unless we're cheeky and use unsafe)
    // maybe some type that lets us create a modified "view" over the original data?

    // copy remaining bytes (expected internal payload) to after the reconstructed internal header)
    tmp.put_slice(buf);

    // advance the inner buf by the bytes we're meant to be reading
    buf.advance(payload_length - 1);

    Ok((version, tmp))
}

/// Encodes an extra version element into an existing RLP message, re-writing the header to include it. YOU MUST USE decode_rlp_version FOR DECODING
pub fn encode_rlp_version(encoded_inner: Vec<u8>, version: u8, out: &mut dyn bytes::BufMut) {
    encode_rlp_version_inner(&mut &encoded_inner[..], version, out);
}

/// inner version that uses &mut &[u8]
/// outer version is for convenience.
/// Encodes an extra version element into an existing RLP message, re-writing the header to include it. YOU MUST USE decode_rlp_version FOR DECODING
pub fn encode_rlp_version_inner(
    encoded_inner: &mut &[u8],
    version: u8,
    out: &mut dyn bytes::BufMut,
) {
    // decode the inner header and re-write it so we include the version
    let alloy_rlp::Header {
        list,
        payload_length,
        // decode advances the slice view past the inner header
    } = alloy_rlp::Header::decode(encoded_inner)
        // inner header is from trusted encode code
        .expect("Should be able to decode inner RLP header");

    // +1 for the version length
    let payload_length = 1 + payload_length;

    alloy_rlp::Header {
        list,
        payload_length,
    }
    .encode(out);
    // encode version, then the remaining inner payload
    alloy_rlp::Encodable::encode(&version, out);
    // only encode the remaining buf, minus the inner header
    out.put_slice(encoded_inner);
}

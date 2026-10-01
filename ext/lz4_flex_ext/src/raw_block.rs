use lz4_flex::block::{compress_into, decompress_into, get_maximum_output_size};
use magnus::{Error, RString};

use crate::{decode_error_class, encode_error_class, nogvl_if_large, LockedRString, RStringMut};

pub(crate) fn compress(input: LockedRString) -> Result<RString, Error> {
    let input_slice = input.as_slice();
    let capacity = get_maximum_output_size(input_slice.len());
    let mut output = RStringMut::zeroed(capacity)?;
    let output_slice = output.as_mut_slice();
    let size = nogvl_if_large(capacity, || compress_into(input_slice, output_slice))?
        .map_err(|error| Error::new(encode_error_class(), error.to_string()))?;

    output.set_len(size);
    Ok(output.into_inner())
}

pub(crate) fn decompress(input: LockedRString, max_output_size: usize) -> Result<RString, Error> {
    let input_slice = input.as_slice();
    let mut output = RStringMut::zeroed(max_output_size)?;
    let output_slice = output.as_mut_slice();
    let size = nogvl_if_large(input_slice.len().max(max_output_size), || {
        decompress_into(input_slice, output_slice)
    })?
    .map_err(|error| Error::new(decode_error_class(), error.to_string()))?;

    output.set_len(size);
    Ok(output.into_inner())
}
